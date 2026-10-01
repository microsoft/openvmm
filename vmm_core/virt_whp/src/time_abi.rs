// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVX time ABI v1 on WHP.
//!
//! Active only for partitions built with
//! [`ProtoPartitionConfig::time_abi`](virt::ProtoPartitionConfig::time_abi).
//! It implements the WHP column of "Backend obligations" in NVX
//! `doc/design/time-abi.md`:
//!
//! - **Partition.** No Hyper-V guest interface (so no synthetic processor
//!   features), the in-hypervisor (offloaded) APIC, no isolation, no VTL2,
//!   and no nested virtualization. Without synthetic features WHP reports no
//!   hypervisor CPUID leaf and serves no synthetic MSR of its own.
//! - **Processor features.** Both feature banks are set explicitly to every
//!   feature WHP offers, without the TSC-deadline timer, `IA32_TSC_ADJUST`,
//!   or APERF/MPERF. WHP's default banks omit speculation controls that the
//!   host offers (SPEC_CTRL, STIBP, and SSBD on Skylake-SP, PSFD on Ice Lake),
//!   so they are never used. The CPU profiles will supply the banks.
//! - **CPUID.** The topology leaves and every leaf of the time ABI CPUID exit
//!   to OpenVMM, which answers from the partition's CPUID results; the time
//!   ABI CPUID applies after every other source. The explicit zero leaves of
//!   the identity range do not exit: WHP answers the whole hypervisor range
//!   with zeros natively, which preflight samples. Each exit-list entry costs
//!   about 25 µs of partition setup, and each native CPUID read about 18 µs.
//! - **Invariant TSC.** The time ABI CPUID sets bit 8 of CPUID `0x80000007`
//!   EDX on every host. Azure's WHP offers no `TscInvariantSupport` to its L1
//!   partitions, so there the bit comes from the CPUID result alone; host
//!   qualification measures the invariance.
//! - **Identity MSRs.** `UnhandledMsrs` exits deliver every access to an MSR
//!   that WHP does not implement, which without synthetic features includes
//!   the whole identity range, to [`TimeAbiMsrs`] before any other MSR
//!   handling. With their feature bits clear, WHP does not implement
//!   `IA32_TSC_ADJUST` and `IA32_TSC_DEADLINE` either; they raise #GP, and
//!   so do the legacy L2-cache MSRs that the backend stubs for the PCAT BIOS.
//! - **Rates.** The guest TSC runs at the host rate: OpenVMM never sets the
//!   `ProcessorClockFrequency` partition property. The offloaded APIC timer
//!   runs at WHP's fixed 200 MHz (WHP cannot set `InterruptClockFrequency`).
//! - **Synchronized TSC set.** Partition time is suspended, the target is
//!   written to every VP and read back while frozen, and partition time
//!   resumes explicitly before any VP runs, so the guest TSC runs from the
//!   restore anchor. Every VP exists from partition creation, so no VP can be
//!   created after the set.
//!
//! These semantics were measured on Windows Server 2025 bare metal (Xeon Gold
//! 6138, Skylake-SP) and on Azure runners (Xeon Platinum 8370C and 8573C).

use crate::WhpPartitionInner;
use crate::WhpProcessor;
use crate::WhpResultExt;
use crate::profile_features::WhpFeatures;
use crate::profile_features::profile_features;
use cpu_profile::CpuProfile;
use inspect::Inspect;
use std::sync::Arc;
use std::sync::OnceLock;
use virt::CpuidLeaf;
use virt::CpuidLeafSet;
use virt::IsolationType;
use virt::VpIndex;
use virt::time_abi::BackendPreflight;
use virt::time_abi::HostTimeSample;
use virt::time_abi::IdentityMsrRoute;
use virt::time_abi::MAX_ANCHOR_PAIRING_NS;
use virt::time_abi::TimeAbiBackend;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiConfig;
use virt::time_abi::TimeAbiError;
use virt::time_abi::TimeAbiMsrs;
use virt::time_abi::TscAnchor;
use virt::time_abi::TscSetReport;
use virt::time_abi::TscSyncMethod;
use virt::time_abi::host::sample_host_time;
use virt::time_abi::identity::HYPERVISOR_CPUID_RANGE;
use virt::time_abi::identity::IDENTITY_CPUID_RANGE;
use virt::time_abi::identity::IDENTITY_MAX_LEAF;
use whp::abi::WHV_EXTENDED_VM_EXITS;
use whp::abi::WHV_PROCESSOR_FEATURES;
use whp::abi::WHV_PROCESSOR_FEATURES1;
use whp::abi::WHV_PROCESSOR_XSAVE_FEATURES;
use whp::abi::WHV_X64_MSR_EXIT_BITMAP;
use x86defs::cpuid::CpuidFunction;

/// `IA32_TSC_ADJUST`, which the time ABI hides.
const MSR_IA32_TSC_ADJUST: u32 = 0x3b;

/// `IA32_TSC_DEADLINE`, which the time ABI hides.
const MSR_IA32_TSC_DEADLINE: u32 = x86defs::X86X_MSR_TSC_DEADLINE;

/// The legacy P6 L2-cache MSRs that the backend stubs for Windows booting
/// from the PCAT BIOS (`MYSTERY_MSRS` in `vp.rs`). No CPU profile pins them
/// and a microVM never probes them, so they raise #GP on a time ABI
/// partition, as on MSHV.
const LEGACY_L2_CACHE_MSRS: [u32; 9] = [0x88, 0x89, 0x8a, 0x116, 0x118, 0x119, 0x11a, 0x11b, 0x11e];

/// Leaves whose results OpenVMM computes from the topology on every vendor
/// (see [`virt::x86::topology::topology_cpuid`]) and adjusts per VP.
const TOPOLOGY_CPUID_EXITS: [u32; 3] = [
    CpuidFunction::VersionAndFeatures.0,
    CpuidFunction::ExtendedTopologyEnumeration.0,
    CpuidFunction::V2ExtendedTopologyEnumeration.0,
];

/// Topology leaves OpenVMM computes only on Intel-compatible hosts.
const INTEL_TOPOLOGY_CPUID_EXITS: [u32; 1] = [CpuidFunction::CacheParameters.0];

/// Topology leaves OpenVMM computes only on AMD-compatible hosts.
const AMD_TOPOLOGY_CPUID_EXITS: [u32; 2] = [
    CpuidFunction::ExtendedAddressSpaceSizes.0,
    CpuidFunction::ProcessorTopologyDefinition.0,
];

/// The feature-bank bits the time ABI clears: the TSC-deadline timer,
/// `IA32_TSC_ADJUST`, and APERF/MPERF.
const HIDDEN_FEATURES1: WHV_PROCESSOR_FEATURES1 = WHV_PROCESSOR_FEATURES1(
    WHV_PROCESSOR_FEATURES1::TscDeadlineTmrSupport.0
        | WHV_PROCESSOR_FEATURES1::TscAdjustSupport.0
        | WHV_PROCESSOR_FEATURES1::ACountMCountSupport.0,
);

/// Leaves of the hypervisor range that preflight reads natively, to check
/// that WHP reports nothing there: the first leaf past the identity, the
/// Hyper-V virtualization stack interface leaf (where `"VS#1"` would
/// appear), the last identity leaf, and three hypervisor signature bases.
/// The explicit zero leaves among them stand for the rest, which WHP answers
/// the same way, and the guest's boot check reads them all.
const PREFLIGHT_SENTINEL_LEAVES: [u32; 6] = [
    0x4000_0006,
    0x4000_0081,
    0x4000_00ff,
    0x4000_0100,
    0x4000_0200,
    0x4000_ff00,
];

/// Leaves the effective CPUID reports besides the time ABI CPUID: the
/// maximum basic and extended leaves.
const EFFECTIVE_CPUID_MAX_LEAVES: [u32; 2] = [
    CpuidFunction::VendorAndMaxFunction.0,
    CpuidFunction::ExtendedMaxFunction.0,
];

/// `OSXSAVE` in CPUID `0x1` ECX: guest CR4 state, not configuration.
const ECX1_OSXSAVE: u32 = 1 << 27;
/// `OSPKE` in CPUID `0x7.0` ECX: guest CR4 state, not configuration.
const ECX7_OSPKE: u32 = 1 << 4;

/// How many bracketed reads of VP 0's TSC a capture anchor takes at most. A
/// nested register read takes 17 to 25 µs and only an occasional one pairs
/// within the bound, so the search tries as many as MSHV does; capture
/// latency is not on the restore path.
const ANCHOR_ATTEMPTS: usize = 64;

/// A capture anchor paired this tightly ends the search early. A WHP
/// register read takes 9.5 to 11 µs on bare metal and 17 to 25 µs nested.
const ANCHOR_TARGET_PAIRING_NS: u64 = 6_000;

/// Time ABI state of a WHP partition.
#[derive(Inspect)]
pub(crate) struct WhpTimeAbi {
    /// The identity MSR handler, shared with the worker.
    #[inspect(
        rename = "tsc_invariant_control",
        with = "|x| x.tsc_invariant_control()"
    )]
    msrs: Arc<TimeAbiMsrs>,
    /// The time ABI CPUID.
    #[inspect(skip)]
    cpuid: Arc<CpuidLeafSet>,
    /// The CPUID exit list.
    #[inspect(with = "|x| format!(\"{x:#x?}\")")]
    cpuid_exits: Vec<u32>,
    /// Processor feature bank 0.
    #[inspect(hex)]
    features_bank0: u64,
    /// Processor feature bank 1.
    #[inspect(hex)]
    features_bank1: u64,
    /// The XSAVE features, when a CPU profile set them.
    #[inspect(hex)]
    features_xsave: Option<u64>,
    /// The CPU profile the features derive from, if any.
    cpu_profile: Option<String>,
    /// The effective CPUID record, computed once: it depends only on the
    /// configuration, because it excludes guest state.
    #[inspect(skip)]
    effective: OnceLock<Vec<CpuidLeaf>>,
}

impl WhpTimeAbi {
    /// Programs a new WHP partition for the time ABI: the CPUID exits, the
    /// processor features, and the unhandled-MSR exits. The partition must
    /// have passed [`validate_partition`].
    ///
    /// With a CPU `profile`, the processor feature banks and the XSAVE
    /// features derive from it ([`profile_features`]); without one, the banks
    /// are WHP's capability without the hidden time features.
    pub(crate) fn configure(
        config: &TimeAbiConfig,
        profile: Option<&CpuProfile>,
        whp_config: &mut whp::PartitionConfig,
        extended_exits: &mut WHV_EXTENDED_VM_EXITS,
    ) -> Result<Self, TimeAbiError> {
        let amd = match whp::capabilities::processor_vendor() {
            Ok(whp::abi::WHvProcessorVendorIntel) => false,
            Ok(whp::abi::WHvProcessorVendorAmd | whp::abi::WHvProcessorVendorHygon) => true,
            Ok(_) => return Err(routing_error("the processor vendor is not x86")),
            Err(err) => {
                return Err(routing_error(format!(
                    "cannot query the processor vendor: {err}"
                )));
            }
        };

        let available_exits = whp::capabilities::extended_vm_exits()
            .map_err(|err| routing_error(format!("cannot query the extended VM exits: {err}")))?;
        let exits = WHV_EXTENDED_VM_EXITS::X64CpuidExit | WHV_EXTENDED_VM_EXITS::X64MsrExit;
        if available_exits.0 & exits.0 != exits.0 {
            return Err(routing_error(format!(
                "WHP lacks CPUID or MSR exits (extended VM exits {:#x})",
                available_exits.0
            )));
        }
        *extended_exits |= exits;

        let cpuid_exits = cpuid_exits(amd, &config.cpuid);
        whp_config
            .set_property(whp::PartitionProperty::CpuidExitList(&cpuid_exits))
            .map_err(|err| routing_error(format!("cannot set the CPUID exit list: {err}")))?;

        let available = whp::capabilities::processor_features().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("cannot query the processor features: {err}"),
            )
        })?;
        let (features, xsave) = match profile {
            Some(profile) => {
                let available_xsave =
                    whp::capabilities::processor_xsave_features().map_err(|err| {
                        TimeAbiError::new(
                            TimeAbiCode::ProfileUnsupported,
                            format!("cannot query the XSAVE features: {err}"),
                        )
                    })?;
                let derived = profile_features(
                    profile,
                    WhpFeatures {
                        banks: [available.bank0.0, available.bank1.0],
                        xsave: available_xsave.0,
                    },
                )?;
                let mut features = available;
                features.bank0 = WHV_PROCESSOR_FEATURES(derived.banks[0]);
                features.bank1 = WHV_PROCESSOR_FEATURES1(derived.banks[1]);
                (processor_features(features), Some(derived.xsave))
            }
            None => (processor_features(available), None),
        };
        whp_config
            .set_property(whp::PartitionProperty::ProcessorFeaturesBanks(features))
            .map_err(|err| {
                TimeAbiError::new(
                    TimeAbiCode::ProfileUnsupported,
                    format!(
                        "cannot set processor feature banks {:#x} and {:#x}: {err}",
                        features.bank0.0, features.bank1.0
                    ),
                )
            })?;
        if let Some(xsave) = xsave {
            whp_config
                .set_property(whp::PartitionProperty::ProcessorXsaveFeatures(
                    WHV_PROCESSOR_XSAVE_FEATURES(xsave),
                ))
                .map_err(|err| {
                    TimeAbiError::new(
                        TimeAbiCode::ProfileUnsupported,
                        format!("cannot set XSAVE features {xsave:#x}: {err}"),
                    )
                })?;
        }

        let msr_exits = whp::capabilities::x64_msr_exit_bitmap()
            .map_err(|err| routing_error(format!("cannot query the MSR exits: {err}")))?;
        if !msr_exits.is_set(WHV_X64_MSR_EXIT_BITMAP::UnhandledMsrs) {
            return Err(routing_error(format!(
                "WHP lacks unhandled-MSR exits (MSR exit bitmap {:#x})",
                msr_exits.0
            )));
        }
        whp_config
            .set_property(whp::PartitionProperty::X64MsrExitBitmap(
                WHV_X64_MSR_EXIT_BITMAP::UnhandledMsrs,
            ))
            .map_err(|err| routing_error(format!("cannot enable unhandled-MSR exits: {err}")))?;

        tracing::info!(
            cpuid_exits = cpuid_exits.len(),
            cpu_profile = profile.map(|profile| profile.id()),
            available_bank0 = format_args!("{:#x}", available.bank0.0),
            available_bank1 = format_args!("{:#x}", available.bank1.0),
            bank0 = format_args!("{:#x}", features.bank0.0),
            bank1 = format_args!("{:#x}", features.bank1.0),
            xsave = xsave.map(|xsave| format!("{xsave:#x}")),
            tsc_invariant_feature = features
                .bank1
                .is_set(WHV_PROCESSOR_FEATURES1::TscInvariantSupport),
            "time ABI: WHP partition configured"
        );

        Ok(Self {
            msrs: config.msrs.clone(),
            cpuid: config.cpuid.clone(),
            cpuid_exits,
            features_bank0: features.bank0.0,
            features_bank1: features.bank1.0,
            features_xsave: xsave,
            cpu_profile: profile.map(|profile| profile.id().to_owned()),
            effective: OnceLock::new(),
        })
    }

    /// Returns the CPUID results of the partition (see [`partition_cpuid`]),
    /// after checking that every result reaches the guest
    /// ([`check_cpuid_delivery`]).
    pub(crate) fn partition_cpuid(
        &self,
        own: Vec<CpuidLeaf>,
    ) -> Result<CpuidLeafSet, TimeAbiError> {
        let cpuid = partition_cpuid(own, &self.cpuid)?;
        check_cpuid_delivery(&cpuid, &self.cpuid_exits)?;
        Ok(cpuid)
    }
}

fn routing_error(message: impl Into<String>) -> TimeAbiError {
    TimeAbiError::new(TimeAbiCode::IdentityRouting, message)
}

/// Fails unless the partition can carry the time ABI: no isolation, no
/// Hyper-V guest interface (whose synthetic processor features would serve
/// the identity CPUID and MSRs natively, and which VTL2 requires), no nested
/// virtualization, and the offloaded APIC.
pub(crate) fn validate_partition(
    isolation: IsolationType,
    hv_configured: bool,
    nested_virt: bool,
    user_mode_apic: bool,
) -> Result<(), TimeAbiError> {
    if isolation != IsolationType::None {
        return Err(routing_error(format!(
            "the WHP time ABI does not support {isolation:?} isolation"
        )));
    }
    if hv_configured {
        return Err(routing_error(
            "the WHP time ABI cannot be combined with the Hyper-V guest interface",
        ));
    }
    if nested_virt {
        return Err(routing_error(
            "the WHP time ABI cannot be combined with nested virtualization",
        ));
    }
    if user_mode_apic {
        return Err(TimeAbiError::new(
            TimeAbiCode::LapicRateUnavailable,
            "the WHP time ABI requires the offloaded APIC, not the user-mode APIC",
        ));
    }
    Ok(())
}

/// Returns the CPUID exit list of a time ABI partition: the topology leaves
/// of the host's vendor and every leaf of the time ABI CPUID except the
/// explicit zero leaves, sorted and unique.
pub(crate) fn cpuid_exits(amd: bool, config: &CpuidLeafSet) -> Vec<u32> {
    let mut exits = TOPOLOGY_CPUID_EXITS.to_vec();
    if amd {
        exits.extend(AMD_TOPOLOGY_CPUID_EXITS);
    } else {
        exits.extend(INTEL_TOPOLOGY_CPUID_EXITS);
    }
    exits.extend(
        config
            .leaves()
            .iter()
            .filter(|leaf| !is_zero_fill(leaf))
            .map(|leaf| leaf.function),
    );
    exits.sort_unstable();
    exits.dedup();
    exits
}

/// Returns the processor features of a time ABI partition: `available`
/// without the TSC-deadline timer, `IA32_TSC_ADJUST`, and APERF/MPERF. The
/// invariant TSC stays where WHP offers it.
pub(crate) fn processor_features(available: whp::ProcessorFeatures) -> whp::ProcessorFeatures {
    let mut features = available;
    features.bank1 = WHV_PROCESSOR_FEATURES1(features.bank1.0 & !HIDDEN_FEATURES1.0);
    features
}

/// Returns whether `leaf` is an explicit zero leaf past the identity, which
/// WHP answers with zeros natively when it does not exit.
fn is_zero_fill(leaf: &CpuidLeaf) -> bool {
    leaf.function > IDENTITY_MAX_LEAF
        && IDENTITY_CPUID_RANGE.contains(&leaf.function)
        && leaf.index.is_none()
        && leaf.result == [0; 4]
        && leaf.mask == [!0; 4]
}

/// Returns the CPUID results of a time ABI partition: `own`, the backend's
/// other results, without their hypervisor-range leaves, then `config`, so
/// that the time ABI CPUID overrides every other source.
///
/// Fails with `E_IDENTITY_ROUTING` if another result would shadow part of
/// `config`: [`CpuidLeafSet::result`] applies only the first matching leaf,
/// so a subleaf-less leaf would hide a subleaf of `config`.
pub(crate) fn partition_cpuid(
    own: Vec<CpuidLeaf>,
    config: &CpuidLeafSet,
) -> Result<CpuidLeafSet, TimeAbiError> {
    let mut leaves: Vec<CpuidLeaf> = own
        .into_iter()
        .filter(|leaf| !HYPERVISOR_CPUID_RANGE.contains(&leaf.function))
        .collect();
    leaves.extend(config.leaves().iter().copied());
    let cpuid = CpuidLeafSet::new(leaves);
    for leaf in config.leaves() {
        let index = leaf.index.unwrap_or(0);
        for base in [[0; 4], [!0; 4]] {
            let result = cpuid.result(leaf.function, index, &base);
            let applied = result
                .iter()
                .zip(leaf.result)
                .zip(leaf.mask)
                .all(|((actual, expected), mask)| actual & mask == expected & mask);
            if !applied {
                return Err(routing_error(format!(
                    "another CPUID result shadows time ABI leaf {:#x}/{index:#x}",
                    leaf.function
                )));
            }
        }
    }
    Ok(cpuid)
}

/// Checks that every leaf of `cpuid` reaches the guest: each leaf exits,
/// except the explicit zero leaves, which WHP answers natively. Fails with
/// `E_IDENTITY_ROUTING`, naming the first leaf that would not.
pub(crate) fn check_cpuid_delivery(
    cpuid: &CpuidLeafSet,
    exits: &[u32],
) -> Result<(), TimeAbiError> {
    if let Some(leaf) = cpuid
        .leaves()
        .iter()
        .find(|leaf| !is_zero_fill(leaf) && exits.binary_search(&leaf.function).is_err())
    {
        return Err(routing_error(format!(
            "CPUID leaf {:#x} has a programmed result but does not exit",
            leaf.function
        )));
    }
    Ok(())
}

/// Clears the bits of a CPUID result that reflect guest state rather than
/// configuration: `OSXSAVE` and `OSPKE` follow the VP's CR4, and the XSAVE
/// area sizes in `0xD.0` and `0xD.1` EBX follow its XCR0 and XSS.
fn normalize_cpuid(function: u32, index: u32, mut result: [u32; 4]) -> [u32; 4] {
    match CpuidFunction(function) {
        CpuidFunction::VersionAndFeatures => result[2] &= !ECX1_OSXSAVE,
        CpuidFunction::ExtendedFeatures if index == 0 => result[2] &= !ECX7_OSPKE,
        CpuidFunction::ExtendedStateEnumeration if index <= 1 => result[1] = 0,
        _ => {}
    }
    result
}

/// Returns the effective CPUID record: the results VP 0 sees, from `vp0`,
/// for the maximum basic and extended leaves and for every leaf the
/// partition programs, `cpuid` (the topology, profile, and identity leaves),
/// with guest-state bits cleared. The explicit zero leaves are reported as
/// programmed; preflight samples them natively.
pub(crate) fn effective_cpuid<E>(
    cpuid: &CpuidLeafSet,
    mut vp0: impl FnMut(u32, u32) -> Result<[u32; 4], E>,
) -> Result<Vec<CpuidLeaf>, E> {
    let mut leaves = Vec::with_capacity(EFFECTIVE_CPUID_MAX_LEAVES.len() + cpuid.leaves().len());
    for function in EFFECTIVE_CPUID_MAX_LEAVES {
        leaves.push(CpuidLeaf::new(function, vp0(function, 0)?));
    }
    for leaf in cpuid.leaves() {
        let index = leaf.index.unwrap_or(0);
        let result = if is_zero_fill(leaf) {
            leaf.result
        } else {
            normalize_cpuid(leaf.function, index, vp0(leaf.function, index)?)
        };
        let effective = CpuidLeaf::new(leaf.function, result);
        leaves.push(match leaf.index {
            Some(index) => effective.indexed(index),
            None => effective,
        });
    }
    Ok(CpuidLeafSet::new(leaves).into_leaves())
}

/// Checks that WHP answers the [`PREFLIGHT_SENTINEL_LEAVES`] with zeros,
/// reading each with `native`. Fails with `E_IDENTITY_ROUTING`.
pub(crate) fn check_native_sentinels<E: std::fmt::Display>(
    mut native: impl FnMut(u32) -> Result<[u32; 4], E>,
) -> Result<(), TimeAbiError> {
    for function in PREFLIGHT_SENTINEL_LEAVES {
        let result = native(function)
            .map_err(|err| routing_error(format!("cannot read CPUID {function:#x}: {err}")))?;
        if result != [0; 4] {
            return Err(routing_error(format!(
                "WHP reports CPUID {function:#x} as {result:#x?} instead of zeros"
            )));
        }
    }
    Ok(())
}

/// Checks that the processor feature banks took effect: WHP's own CPUID
/// results, before the time ABI CPUID applies, must offer neither the
/// TSC-deadline timer, `IA32_TSC_ADJUST`, nor APERF/MPERF, or the guest
/// could use their MSRs. `native` reads WHP's result for a leaf and subleaf.
/// Fails with `E_PROFILE_UNSUPPORTED`.
pub(crate) fn check_feature_banks<E: std::fmt::Display>(
    mut native: impl FnMut(u32, u32) -> Result<[u32; 4], E>,
) -> Result<(), TimeAbiError> {
    const CHECKS: [(u32, usize, u32, &str); 3] = [
        (0x1, 2, 1 << 24, "the TSC-deadline timer"),
        (0x6, 2, 1 << 0, "APERF/MPERF"),
        (0x7, 1, 1 << 1, "IA32_TSC_ADJUST"),
    ];
    for (function, register, bit, feature) in CHECKS {
        let result = native(function, 0).map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("cannot read CPUID {function:#x}: {err}"),
            )
        })?;
        if result[register] & bit != 0 {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("the processor feature banks still offer {feature}"),
            ));
        }
    }
    Ok(())
}

/// Checks that the guest TSC runs at the host rate: the partition's
/// `ProcessorClockFrequency` must equal WHP's capability. Fails with
/// `E_TSC_SCALING_ACTIVE`.
pub(crate) fn check_unscaled(partition_hz: u64, capability_hz: u64) -> Result<(), TimeAbiError> {
    if partition_hz != capability_hz {
        return Err(TimeAbiError::new(
            TimeAbiCode::TscScalingActive,
            format!(
                "the partition TSC runs at {partition_hz} Hz, but the host TSC runs at {capability_hz} Hz"
            ),
        ));
    }
    Ok(())
}

/// An access to an MSR that exited to OpenVMM.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MsrAccess {
    Read,
    Write(u64),
}

/// How a time ABI MSR access completes.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MsrOutcome {
    /// The read returns this value.
    Read(u64),
    /// The write takes effect.
    Written,
    /// The access raises #GP.
    Fault,
}

/// Serves an MSR access that the time ABI owns: the identity range through
/// `msrs`, and the MSRs the time ABI hides, which raise #GP. Returns `None`
/// for every other MSR, which the backend handles as before.
pub(crate) fn serve_msr(
    msrs: &TimeAbiMsrs,
    vp: VpIndex,
    msr: u32,
    access: MsrAccess,
) -> Option<MsrOutcome> {
    if matches!(msr, MSR_IA32_TSC_ADJUST | MSR_IA32_TSC_DEADLINE) {
        tracelimit::info_ratelimited!(
            vp = vp.index(),
            msr,
            ?access,
            "time ABI: hidden timer MSR access raises #GP"
        );
        return Some(MsrOutcome::Fault);
    }
    if LEGACY_L2_CACHE_MSRS.contains(&msr) {
        return Some(MsrOutcome::Fault);
    }
    let outcome = match access {
        MsrAccess::Read => msrs.read(vp, msr)?.map(MsrOutcome::Read),
        MsrAccess::Write(value) => msrs.write(vp, msr, value)?.map(|()| MsrOutcome::Written),
    };
    Some(outcome.unwrap_or(MsrOutcome::Fault))
}

/// One bracketed read of VP 0's TSC: host samples taken immediately before
/// and after the read.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct AnchorCandidate {
    tsc: u64,
    before: HostTimeSample,
    after: HostTimeSample,
}

impl AnchorCandidate {
    /// The host monotonic time the read took, in nanoseconds.
    fn width_ns(&self) -> u64 {
        self.after
            .monotonic_ns
            .saturating_sub(self.before.monotonic_ns)
    }

    /// Pairs the TSC with the midpoint of the bracket, which differs from the
    /// read instant by at most half the bracket. UTC is derived from the
    /// first sample and the monotonic midpoint, so a UTC step inside the
    /// bracket does not skew it.
    fn anchor(&self) -> TscAnchor {
        let half = self.width_ns() / 2;
        TscAnchor {
            tsc: self.tsc,
            sample: HostTimeSample {
                utc_ns: self.before.utc_ns + half,
                monotonic_ns: self.before.monotonic_ns + half,
            },
            pairing_ns: self.width_ns().div_ceil(2),
        }
    }
}

/// Chooses the tightest candidate. Fails with `E_TSC_ANCHOR` if none pairs
/// within `bound_ns`.
fn select_anchor(candidates: &[AnchorCandidate], bound_ns: u64) -> Result<TscAnchor, TimeAbiError> {
    let best = candidates
        .iter()
        .min_by_key(|candidate| candidate.width_ns())
        .ok_or_else(|| TimeAbiError::new(TimeAbiCode::TscAnchor, "no TSC read was attempted"))?;
    let anchor = best.anchor();
    if anchor.pairing_ns > bound_ns {
        return Err(TimeAbiError::new(
            TimeAbiCode::TscAnchor,
            format!(
                "the tightest of {} VP 0 TSC reads pairs within {} ns, above the {bound_ns} ns bound",
                candidates.len(),
                anchor.pairing_ns
            ),
        ));
    }
    Ok(anchor)
}

/// Checks that every VP read back `target`. Fails with
/// `E_TSC_SYNC_READBACK`, naming the first VP that differs.
fn check_readback(target: u64, readback: &[(VpIndex, u64)]) -> Result<(), TimeAbiError> {
    if let Some((vp, value)) = readback.iter().find(|(_, value)| *value != target) {
        return Err(TimeAbiError::new(
            TimeAbiCode::TscSyncReadback,
            format!(
                "VP {} holds TSC {value:#x} instead of the synchronized {target:#x}",
                vp.index()
            ),
        ));
    }
    Ok(())
}

/// WHP's own CPUID results for VP 0, read on demand and kept for the rest
/// of one check: each read costs about 18 µs.
struct NativeCpuid<'a> {
    partition: &'a whp::Partition,
    results: Vec<((u32, u32), [u32; 4])>,
}

impl<'a> NativeCpuid<'a> {
    fn new(partition: &'a whp::Partition) -> Self {
        Self {
            partition,
            results: Vec::new(),
        }
    }

    fn get(&mut self, function: u32, index: u32) -> Result<[u32; 4], whp::WHvError> {
        if let Some((_, result)) = self
            .results
            .iter()
            .find(|(key, _)| *key == (function, index))
        {
            return Ok(*result);
        }
        let output = self.partition.vp(0).get_cpuid_output(function, index)?;
        let result = [output.Eax, output.Ebx, output.Ecx, output.Edx];
        self.results.push(((function, index), result));
        Ok(result)
    }
}

/// Returns the result VP 0 sees for a leaf: the partition's CPUID results
/// over WHP's own. Every leaf with a programmed result exits (see
/// [`check_cpuid_delivery`]), the exit handler applies the partition's
/// results over WHP's own, and VP 0 carries the BSP identity that the
/// topology results already hold. WHP's result is read only if a bit of it
/// shows through.
fn vp0_cpuid(
    cpuid: &CpuidLeafSet,
    native: &mut NativeCpuid<'_>,
    function: u32,
    index: u32,
) -> Result<[u32; 4], whp::WHvError> {
    let programmed = cpuid
        .leaves()
        .iter()
        .find(|leaf| leaf.matches(function, index));
    let base = match programmed {
        Some(leaf) if leaf.mask == [!0; 4] => [0; 4],
        _ => native.get(function, index)?,
    };
    Ok(cpuid.result(function, index, &base))
}

impl WhpPartitionInner {
    fn time_abi_state(&self) -> Result<&WhpTimeAbi, TimeAbiError> {
        self.time_abi.as_ref().ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::TscSyncUnsupported,
                "the partition was built without the time ABI",
            )
        })
    }

    /// Returns the effective CPUID record, computing it on first use.
    fn effective_record(
        &self,
        state: &WhpTimeAbi,
        native: &mut NativeCpuid<'_>,
    ) -> Result<Vec<CpuidLeaf>, TimeAbiError> {
        if let Some(record) = state.effective.get() {
            return Ok(record.clone());
        }
        let record = effective_cpuid(&self.cpuid, |function, index| {
            vp0_cpuid(&self.cpuid, native, function, index).map_err(|err| {
                TimeAbiError::new(
                    TimeAbiCode::CpuSurface,
                    format!("cannot read CPUID {function:#x}/{index:#x} of VP 0: {err}"),
                )
            })
        })?;
        Ok(state.effective.get_or_init(|| record).clone())
    }

    fn time_abi_vps(&self) -> impl Iterator<Item = VpIndex> + '_ {
        self.vps.iter().map(|vp| vp.vp_info.base.vp_index)
    }
}

impl TimeAbiBackend for WhpPartitionInner {
    fn native_tsc_hz(&self) -> Result<u64, TimeAbiError> {
        let hz = whp::capabilities::processor_clock_frequency().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::TscRateUnavailable,
                format!("cannot query WHvCapabilityCodeProcessorClockFrequency: {err}"),
            )
        })?;
        if hz == 0 {
            return Err(TimeAbiError::new(
                TimeAbiCode::TscRateUnavailable,
                "WHvCapabilityCodeProcessorClockFrequency is zero",
            ));
        }
        Ok(hz)
    }

    fn lapic_hz(&self) -> Result<u64, TimeAbiError> {
        let hz = self.vtl0.whp.apic_frequency().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::LapicRateUnavailable,
                format!("cannot read the InterruptClockFrequency partition property: {err}"),
            )
        })?;
        if hz == 0 {
            return Err(TimeAbiError::new(
                TimeAbiCode::LapicRateUnavailable,
                "the InterruptClockFrequency partition property is zero",
            ));
        }
        Ok(hz)
    }

    fn preflight(&self) -> Result<BackendPreflight, TimeAbiError> {
        let started = std::time::Instant::now();
        let state = self.time_abi_state()?;

        // Partition creation set up the exits, and the build checked that
        // every programmed CPUID result exits. Check that the exits are in
        // place and that WHP answers the leaves that do not exit with zeros.
        let exits =
            self.vtl0.whp.extended_vm_exits().map_err(|err| {
                routing_error(format!("cannot read the extended VM exits: {err}"))
            })?;
        let required = WHV_EXTENDED_VM_EXITS::X64CpuidExit | WHV_EXTENDED_VM_EXITS::X64MsrExit;
        let msr_exits = self
            .vtl0
            .whp
            .x64_msr_exit_bitmap()
            .map_err(|err| routing_error(format!("cannot read the MSR exit bitmap: {err}")))?;
        if exits.0 & required.0 != required.0
            || !msr_exits.is_set(WHV_X64_MSR_EXIT_BITMAP::UnhandledMsrs)
        {
            return Err(routing_error(format!(
                "the partition lacks CPUID, MSR, or unhandled-MSR exits (extended VM exits {:#x}, MSR exits {:#x})",
                exits.0, msr_exits.0
            )));
        }
        let mut native = NativeCpuid::new(&self.vtl0.whp);
        check_native_sentinels(|function| native.get(function, 0))?;
        check_feature_banks(|function, index| native.get(function, index))?;

        // The guest-visible identity and time bits.
        let effective = CpuidLeafSet::new(self.effective_record(state, &mut native)?);
        let mut lookup = |function, index| effective.result(function, index, &[0; 4]);
        virt::time_abi::identity::check_identity(&mut lookup, self.vps.len() as u32)?;
        let invariant_tsc =
            state
                .cpuid
                .result(CpuidFunction::ExtendedPowerManagement.0, 0, &[0; 4])[3]
                & virt::time_abi::identity::INVARIANT_TSC_EDX
                != 0;
        virt::time_abi::identity::check_time_bits(&mut lookup, invariant_tsc)?;

        // The synchronized set suspends partition time. A partition whose
        // VPs never ran keeps its time frozen until the first VP run, so the
        // probe leaves the partition as it was.
        self.vtl0.whp.suspend_time().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::TscSyncUnsupported,
                format!("cannot suspend partition time: {err}"),
            )
        })?;

        let partition_hz = self.vtl0.whp.tsc_frequency().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::TscRateUnavailable,
                format!("cannot read the ProcessorClockFrequency partition property: {err}"),
            )
        })?;
        check_unscaled(partition_hz, self.native_tsc_hz()?)?;

        tracing::info!(
            tsc_hz = partition_hz,
            invariant_tsc,
            tsc_invariant_feature = WHV_PROCESSOR_FEATURES1(state.features_bank1)
                .is_set(WHV_PROCESSOR_FEATURES1::TscInvariantSupport),
            cpuid_reads = native.results.len(),
            elapsed_us = started.elapsed().as_micros() as u64,
            "time ABI: WHP preflight"
        );
        Ok(BackendPreflight {
            msr_route: IdentityMsrRoute::ExitToVmm,
            sync: TscSyncMethod::FrozenWrite,
        })
    }

    fn effective_cpuid(&self) -> Result<Vec<CpuidLeaf>, TimeAbiError> {
        let state = self.time_abi_state()?;
        self.effective_record(state, &mut NativeCpuid::new(&self.vtl0.whp))
    }

    fn capture_anchor(&self) -> Result<TscAnchor, TimeAbiError> {
        self.time_abi_state()?;
        let bsp = self.vtl0.whp.vp(VpIndex::BSP.index());
        let mut candidates = Vec::with_capacity(ANCHOR_ATTEMPTS);
        for _ in 0..ANCHOR_ATTEMPTS {
            let before = sample_host_time()?;
            let tsc = bsp.get_register(whp::Register64::Tsc).map_err(|err| {
                TimeAbiError::new(
                    TimeAbiCode::TscAnchor,
                    format!("cannot read the TSC of VP 0: {err}"),
                )
            })?;
            let after = sample_host_time()?;
            let candidate = AnchorCandidate { tsc, before, after };
            candidates.push(candidate);
            if candidate.anchor().pairing_ns <= ANCHOR_TARGET_PAIRING_NS {
                break;
            }
        }
        let anchor = select_anchor(&candidates, MAX_ANCHOR_PAIRING_NS)?;
        tracing::info!(
            tsc = anchor.tsc,
            pairing_ns = anchor.pairing_ns,
            attempts = candidates.len(),
            "time ABI: capture anchor"
        );
        Ok(anchor)
    }

    fn set_synchronized_tsc(
        &self,
        target: &mut dyn FnMut(&HostTimeSample) -> Result<u64, TimeAbiError>,
    ) -> Result<TscSetReport, TimeAbiError> {
        let started = std::time::Instant::now();
        self.time_abi_state()?;
        let partition = &self.vtl0.whp;

        // While partition time is suspended, every VP's TSC holds its last
        // value, so the frozen read-back is exact. Writes while time runs
        // would skew the VPs by the 10 to 25 µs each register write takes.
        partition.suspend_time().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::TscSyncUnsupported,
                format!("cannot suspend partition time: {err}"),
            )
        })?;
        let suspended = started.elapsed();

        let sample = sample_host_time()?;
        let target = target(&sample)?;

        let failed = |vp: VpIndex, what: &str, err: whp::WHvError| {
            TimeAbiError::new(
                TimeAbiCode::TscSyncReadback,
                format!("cannot {what} the TSC of VP {}: {err}", vp.index()),
            )
        };
        for vp in self.time_abi_vps() {
            partition
                .vp(vp.index())
                .set_register(whp::Register64::Tsc, target)
                .map_err(|err| failed(vp, "write", err))?;
        }
        let written = started.elapsed();
        let readback = self
            .time_abi_vps()
            .map(|vp| {
                partition
                    .vp(vp.index())
                    .get_register(whp::Register64::Tsc)
                    .map(|value| (vp, value))
                    .map_err(|err| failed(vp, "read back", err))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let read = started.elapsed();
        let result = check_readback(target, &readback);

        // Resume before any VP runs, so the guest TSC runs from the restore
        // anchor rather than from the first VP run. A failed set leaves time
        // suspended: the restore is rejected and no VP runs.
        if result.is_ok() {
            partition.resume_time().map_err(|err| {
                TimeAbiError::new(
                    TimeAbiCode::TscSyncUnsupported,
                    format!("cannot resume partition time: {err}"),
                )
            })?;
        }
        tracing::info!(
            vps = readback.len(),
            target,
            readback_equal = result.is_ok(),
            suspend_us = suspended.as_micros() as u64,
            write_us = (written - suspended).as_micros() as u64,
            readback_us = (read - written).as_micros() as u64,
            total_us = started.elapsed().as_micros() as u64,
            "time ABI: synchronized TSC set with partition time suspended"
        );
        result?;
        Ok(TscSetReport {
            target,
            sample,
            readback,
            method: TscSyncMethod::FrozenWrite,
        })
    }
}

impl WhpProcessor<'_> {
    /// Serves an MSR exit that the time ABI owns (see [`serve_msr`]), ahead
    /// of every other MSR handler. Returns `false` on a partition without the
    /// time ABI and for every other MSR.
    pub(crate) fn handle_time_abi_msr(
        &self,
        info: &whp::abi::WHV_X64_MSR_ACCESS_CONTEXT,
        exit: whp::Exit<'_>,
    ) -> Result<bool, crate::Error> {
        let Some(time_abi) = &self.vp.partition.time_abi else {
            return Ok(false);
        };
        let access = if info.AccessInfo.IsWrite() {
            MsrAccess::Write((info.Rax & 0xffff_ffff) | (info.Rdx << 32))
        } else {
            MsrAccess::Read
        };
        let Some(outcome) = serve_msr(&time_abi.msrs, self.vp.index, info.MsrNumber, access) else {
            return Ok(false);
        };
        tracing::trace!(
            vp = self.vp.index.index(),
            msr = info.MsrNumber,
            ?access,
            ?outcome,
            "time ABI MSR access"
        );
        // RDMSR and WRMSR are two bytes long. A faulting access does not
        // retire, so RIP stays.
        let rip = exit.vp_context.Rip.wrapping_add(2);
        match outcome {
            MsrOutcome::Read(value) => whp::set_registers!(
                self.current_whp(),
                [
                    (whp::Register64::Rax, value & 0xffff_ffff),
                    (whp::Register64::Rdx, value >> 32),
                    (whp::Register64::Rip, rip),
                ]
            )
            .for_op("complete a time ABI MSR read")?,
            MsrOutcome::Written => self
                .current_whp()
                .set_register(whp::Register64::Rip, rip)
                .for_op("complete a time ABI MSR write")?,
            MsrOutcome::Fault => {
                let event = hvdef::HvX64PendingExceptionEvent::new()
                    .with_event_pending(true)
                    .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
                    .with_deliver_error_code(true)
                    .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into());
                self.current_whp()
                    .set_register(whp::Register128::PendingEvent, event.into())
                    .for_op("inject #GP for a time ABI MSR access")?;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use virt::time_abi::DeclaredRates;
    use virt::time_abi::identity::check_identity;
    use virt::time_abi::identity::check_time_bits;
    use virt::time_abi::identity::time_abi_cpuid;
    use virt::time_abi::msr::MSR_APIC_FREQUENCY;
    use virt::time_abi::msr::MSR_TSC_FREQUENCY;
    use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
    use virt::time_abi::msr::MSR_VP_INDEX;
    use vm_topology::processor::TopologyBuilder;

    const VP_COUNT: u32 = 8;

    /// WHP's own results on an Intel host without synthetic features: no
    /// hypervisor leaves, and every time bit wrong.
    fn whp_cpuid(function: u32, index: u32) -> [u32; 4] {
        match (function, index) {
            (0, _) => [0x16, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
            (1, _) => [
                0x0005_0654,
                0x0010_0800,
                0x7ffa_fbff | (1 << 24) | ECX1_OSXSAVE,
                0xbfeb_fbff,
            ],
            (6, _) => [0x77, 2, 9, 0],
            (7, 0) => [0, 0xd19f_4fbb, ECX7_OSPKE, 0],
            (0xa, _) => [0x0740_4f04, 0, 0, 0x603],
            (0x15, _) => [2, 0xd4, 0x017d_7840, 0],
            (0x16, _) => [0x0bb8, 0x0e74, 0x64, 0],
            (0x8000_0000, _) => [0x8000_0008, 0, 0, 0],
            (0x8000_0001, _) => [0, 0, 0x121, 0x2c10_0800],
            _ => [0; 4],
        }
    }

    /// Leaves the backend contributes before the time ABI CPUID: the
    /// topology in leaf 1 and a stale hypervisor leaf.
    fn own_leaves() -> Vec<CpuidLeaf> {
        vec![
            CpuidLeaf::new(1, [0, 0x0008_0000, 0, 0]).masked([0, 0x00ff_0000, 0, 0]),
            CpuidLeaf::new(4, [0x1c00_4121, 0, 0, 0]).indexed(0),
            CpuidLeaf::new(0x4000_0000, [0x4000_0001, 0x4b4d_564b, 0x564b_4d56, 0x4d]),
            CpuidLeaf::new(0x4000_0100, [0x4000_0101, 0x4b4d_564b, 0x564b_4d56, 0x4d]),
        ]
    }

    fn key(leaf: &CpuidLeaf) -> (u32, Option<u32>, [u32; 4], [u32; 4]) {
        (leaf.function, leaf.index, leaf.result, leaf.mask)
    }

    fn declared_msrs() -> TimeAbiMsrs {
        let msrs = TimeAbiMsrs::new();
        msrs.declare(DeclaredRates::new(2_194_843_000, 200_000_000).unwrap())
            .unwrap();
        msrs
    }

    #[test]
    fn partition_validation_rejects_unsupported_configurations() {
        validate_partition(IsolationType::None, false, false, false).unwrap();
        for (isolation, hv, nested, user_mode_apic, code) in [
            (
                IsolationType::Vbs,
                false,
                false,
                false,
                TimeAbiCode::IdentityRouting,
            ),
            (
                IsolationType::None,
                true,
                false,
                false,
                TimeAbiCode::IdentityRouting,
            ),
            (
                IsolationType::None,
                false,
                true,
                false,
                TimeAbiCode::IdentityRouting,
            ),
            (
                IsolationType::None,
                false,
                false,
                true,
                TimeAbiCode::LapicRateUnavailable,
            ),
        ] {
            assert_eq!(
                validate_partition(isolation, hv, nested, user_mode_apic)
                    .unwrap_err()
                    .code,
                code
            );
        }
    }

    #[test]
    fn exit_list_covers_the_topology_and_the_time_abi_cpuid() {
        let config = time_abi_cpuid(VP_COUNT, true);
        let mut expected = vec![1, 4, 6, 7, 0xa, 0xb, 0x15, 0x16, 0x1f];
        expected.extend(0x4000_0000..=0x4000_0005);
        expected.extend([0x8000_0001, 0x8000_0007]);
        assert_eq!(cpuid_exits(false, &config), expected);

        let amd = cpuid_exits(true, &config);
        assert!(!amd.contains(&4));
        assert!(amd.contains(&0x8000_0008) && amd.contains(&0x8000_001e));
        assert!(amd.windows(2).all(|pair| pair[0] < pair[1]));
        // WHP answers the explicit zero leaves natively.
        assert!(
            !amd.iter()
                .any(|leaf| (0x4000_0006..=0x4000_00ff).contains(leaf))
        );
    }

    #[test]
    fn feature_banks_hide_exactly_the_time_bits() {
        let mut available = whp::ProcessorFeatures::default();
        available.bank0 = WHV_PROCESSOR_FEATURES(0x1001_f9ff_e7f7_859f);
        available.bank1 = WHV_PROCESSOR_FEATURES1(!0);
        let features = processor_features(available);
        assert_eq!(features.bank0, available.bank0);
        assert_eq!(features.bank1.0, !((1 << 0) | (1 << 17) | (1 << 18)));
        assert!(
            features
                .bank1
                .is_set(WHV_PROCESSOR_FEATURES1::TscInvariantSupport)
        );
        // Azure's 8573C runners offer no invariant TSC; the banks add none.
        available.bank1 = WHV_PROCESSOR_FEATURES1(0xc_0000_0051);
        assert_eq!(processor_features(available).bank1.0, 0xc_0000_0050);
    }

    #[test]
    fn partition_cpuid_puts_the_time_abi_last() {
        let config = time_abi_cpuid(VP_COUNT, true);
        let cpuid = partition_cpuid(own_leaves(), &config).unwrap();
        let mut effective =
            |function, index| cpuid.result(function, index, &whp_cpuid(function, index));
        check_identity(&mut effective, VP_COUNT).unwrap();
        check_time_bits(&mut effective, true).unwrap();
        // The topology survives in the bits the time ABI does not own.
        assert_eq!(effective(1, 0)[1] & 0x00ff_0000, 0x0008_0000);
        assert_eq!(effective(4, 0)[0], 0x1c00_4121);
        // No hypervisor leaf of another source remains.
        assert_eq!(effective(0x4000_0100, 0), [0; 4]);
        assert!(
            cpuid
                .leaves()
                .iter()
                .filter(|leaf| HYPERVISOR_CPUID_RANGE.contains(&leaf.function))
                .all(|leaf| config.leaves().iter().any(|c| key(c) == key(leaf)))
        );
    }

    #[test]
    fn partition_cpuid_rejects_shadowed_subleaves() {
        let mut own = own_leaves();
        // A subleaf-less leaf 7 would hide the time ABI's leaf 7.0.
        own.push(CpuidLeaf::new(7, [0, 1 << 1, 0, 0]).masked([0, 1 << 1, 0, 0]));
        assert_eq!(
            partition_cpuid(own, &time_abi_cpuid(VP_COUNT, true))
                .unwrap_err()
                .code,
            TimeAbiCode::IdentityRouting
        );
    }

    #[test]
    fn every_programmed_leaf_must_exit() {
        let config = time_abi_cpuid(VP_COUNT, true);
        let cpuid = partition_cpuid(own_leaves(), &config).unwrap();
        let exits = cpuid_exits(false, &config);
        check_cpuid_delivery(&cpuid, &exits).unwrap();
        let without: Vec<u32> = exits
            .iter()
            .copied()
            .filter(|&leaf| leaf != 0x8000_0007)
            .collect();
        let error = check_cpuid_delivery(&cpuid, &without).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::IdentityRouting);
        assert!(error.message.contains("0x80000007"), "{error}");
    }

    #[test]
    fn effective_cpuid_reports_every_programmed_leaf() {
        let config = time_abi_cpuid(VP_COUNT, true);
        let cpuid = partition_cpuid(own_leaves(), &config).unwrap();
        let vp0 = |function, index| {
            Ok::<_, std::convert::Infallible>(cpuid.result(
                function,
                index,
                &whp_cpuid(function, index),
            ))
        };
        let record = effective_cpuid(&cpuid, vp0).unwrap();
        let mut keys: Vec<(u32, Option<u32>)> = cpuid
            .leaves()
            .iter()
            .map(|leaf| (leaf.function, leaf.index))
            .chain([(0, None), (0x8000_0000, None)])
            .collect();
        keys.sort();
        assert_eq!(
            record
                .iter()
                .map(|leaf| (leaf.function, leaf.index))
                .collect::<Vec<_>>(),
            keys
        );
        assert!(record.iter().all(|leaf| leaf.mask == [!0; 4]));

        let record = CpuidLeafSet::new(record);
        let mut lookup = |function, index| record.result(function, index, &[0; 4]);
        check_identity(&mut lookup, VP_COUNT).unwrap();
        check_time_bits(&mut lookup, true).unwrap();
        // Guest CR4 state does not reach the record.
        assert_eq!(lookup(1, 0)[2] & ECX1_OSXSAVE, 0);
        assert_eq!(lookup(7, 0)[2] & ECX7_OSPKE, 0);
        assert_eq!(lookup(0, 0), whp_cpuid(0, 0));
        // The topology leaves are part of the record.
        assert_eq!(lookup(4, 0)[0], 0x1c00_4121);
        assert_eq!(lookup(1, 0)[1] & 0x00ff_0000, 0x0008_0000);
    }

    #[test]
    fn effective_cpuid_ignores_guest_cr4_state() {
        let config = time_abi_cpuid(VP_COUNT, true);
        let cpuid = partition_cpuid(own_leaves(), &config).unwrap();
        let record = |cr4_bits: bool| {
            effective_cpuid(&cpuid, |function, index| {
                let mut result = cpuid.result(function, index, &whp_cpuid(function, index));
                match (function, cr4_bits) {
                    (1, false) => result[2] &= !ECX1_OSXSAVE,
                    (7, false) => result[2] &= !ECX7_OSPKE,
                    _ => {}
                }
                Ok::<_, std::convert::Infallible>(result)
            })
            .unwrap()
            .iter()
            .map(key)
            .collect::<Vec<_>>()
        };
        assert_eq!(record(true), record(false));
    }

    #[test]
    fn normalization_clears_only_runtime_state() {
        let all = [!0; 4];
        assert_eq!(normalize_cpuid(1, 0, all), [!0, !0, !ECX1_OSXSAVE, !0]);
        assert_eq!(normalize_cpuid(7, 0, all), [!0, !0, !ECX7_OSPKE, !0]);
        assert_eq!(normalize_cpuid(7, 1, all), all);
        assert_eq!(normalize_cpuid(0xd, 0, all), [!0, 0, !0, !0]);
        assert_eq!(normalize_cpuid(0xd, 1, all), [!0, 0, !0, !0]);
        assert_eq!(normalize_cpuid(0xd, 2, all), all);
        assert_eq!(normalize_cpuid(0x8000_0001, 0, all), all);
    }

    #[test]
    fn effective_cpuid_reports_read_failures() {
        let config = time_abi_cpuid(VP_COUNT, true);
        let error = effective_cpuid(&config, |function, _| {
            if function == 0x8000_0007 {
                Err(function)
            } else {
                Ok(whp_cpuid(function, 0))
            }
        })
        .unwrap_err();
        assert_eq!(error, 0x8000_0007);
    }

    #[test]
    fn native_sentinels_must_read_zero() {
        let mut read = Vec::new();
        check_native_sentinels(|function| {
            read.push(function);
            Ok::<_, String>(whp_cpuid(function, 0))
        })
        .unwrap();
        assert_eq!(read, PREFLIGHT_SENTINEL_LEAVES);
        let config = time_abi_cpuid(VP_COUNT, true);
        for leaf in [0x4000_0006, 0x4000_0081] {
            assert!(
                config
                    .leaves()
                    .iter()
                    .any(|c| c.function == leaf && is_zero_fill(c))
            );
        }

        for (leaf, result) in [
            (0x4000_0081, [0x3123_5356, 0, 0, 0]),
            (0x4000_0100, [0x4000_0101, 0x4b4d_564b, 0x564b_4d56, 0x4d]),
        ] {
            let error = check_native_sentinels(|function| {
                Ok::<_, String>(if function == leaf { result } else { [0; 4] })
            })
            .unwrap_err();
            assert_eq!(error.code, TimeAbiCode::IdentityRouting);
        }
        assert_eq!(
            check_native_sentinels(|_| Err("no VP")).unwrap_err().code,
            TimeAbiCode::IdentityRouting
        );
    }

    #[test]
    fn feature_banks_must_hide_the_timer_msrs() {
        assert_eq!(
            check_feature_banks(|function, index| Ok::<_, String>(whp_cpuid(function, index)))
                .unwrap_err()
                .code,
            TimeAbiCode::ProfileUnsupported
        );
        let hidden = |function, index| {
            let mut result = whp_cpuid(function, index);
            match function {
                1 => result[2] &= !(1 << 24),
                6 => result[2] &= !1,
                7 => result[1] &= !(1 << 1),
                _ => {}
            }
            Ok::<_, String>(result)
        };
        check_feature_banks(hidden).unwrap();
        for (leaf, register, bit) in [(1, 2, 1 << 24), (6, 2, 1), (7, 1, 1 << 1)] {
            let error = check_feature_banks(|function, index| {
                let mut result = hidden(function, index)?;
                if function == leaf {
                    result[register] |= bit;
                }
                Ok::<_, String>(result)
            })
            .unwrap_err();
            assert_eq!(error.code, TimeAbiCode::ProfileUnsupported, "{leaf:#x}");
        }
    }

    #[test]
    fn scaling_is_detected() {
        check_unscaled(2_194_843_000, 2_194_843_000).unwrap();
        assert_eq!(
            check_unscaled(1_000_000_000, 2_194_843_000)
                .unwrap_err()
                .code,
            TimeAbiCode::TscScalingActive
        );
    }

    #[test]
    fn msr_exits_serve_the_identity_and_hide_the_timer_msrs() {
        let msrs = declared_msrs();
        let read = |vp, msr| serve_msr(&msrs, VpIndex::new(vp), msr, MsrAccess::Read);
        let write = |msr, value| serve_msr(&msrs, VpIndex::BSP, msr, MsrAccess::Write(value));
        assert_eq!(read(5, MSR_VP_INDEX), Some(MsrOutcome::Read(5)));
        assert_eq!(
            read(0, MSR_TSC_FREQUENCY),
            Some(MsrOutcome::Read(2_194_843_000))
        );
        assert_eq!(
            read(1, MSR_APIC_FREQUENCY),
            Some(MsrOutcome::Read(200_000_000))
        );
        assert_eq!(
            read(2, MSR_TSC_INVARIANT_CONTROL),
            Some(MsrOutcome::Read(0))
        );
        assert_eq!(
            write(MSR_TSC_INVARIANT_CONTROL, 1),
            Some(MsrOutcome::Written)
        );
        assert_eq!(
            read(7, MSR_TSC_INVARIANT_CONTROL),
            Some(MsrOutcome::Read(1))
        );
        assert_eq!(
            write(MSR_TSC_INVARIANT_CONTROL, 0),
            Some(MsrOutcome::Written)
        );
        assert_eq!(write(MSR_TSC_INVARIANT_CONTROL, 2), Some(MsrOutcome::Fault));
        for msr in [MSR_VP_INDEX, MSR_TSC_FREQUENCY, MSR_APIC_FREQUENCY] {
            assert_eq!(write(msr, 0), Some(MsrOutcome::Fault), "{msr:#x}");
        }
        // Undefined identity MSRs, such as the hypercall MSRs, fault.
        for msr in [
            0x4000_0000,
            0x4000_0001,
            0x4000_0020,
            0x4000_0073,
            0x4000_01ff,
        ] {
            assert_eq!(read(0, msr), Some(MsrOutcome::Fault), "{msr:#x}");
            assert_eq!(write(msr, 1), Some(MsrOutcome::Fault), "{msr:#x}");
        }
        for msr in [MSR_IA32_TSC_ADJUST, MSR_IA32_TSC_DEADLINE] {
            assert_eq!(read(0, msr), Some(MsrOutcome::Fault), "{msr:#x}");
            assert_eq!(write(msr, 0), Some(MsrOutcome::Fault), "{msr:#x}");
        }
        // The legacy L2-cache MSRs that the backend otherwise stubs fault.
        for msr in [0x88, 0x89, 0x8a, 0x116, 0x118, 0x119, 0x11a, 0x11b, 0x11e] {
            assert_eq!(read(0, msr), Some(MsrOutcome::Fault), "{msr:#x}");
            assert_eq!(write(msr, 0), Some(MsrOutcome::Fault), "{msr:#x}");
        }
        // Every other MSR keeps its existing handling.
        for msr in [0x10, 0x1b, 0x3a, 0x6df, 0x6e1, 0x3fff_ffff, 0x4000_0200] {
            assert_eq!(read(0, msr), None, "{msr:#x}");
            assert_eq!(write(msr, 0), None, "{msr:#x}");
        }
    }

    #[test]
    fn frequency_reads_fault_before_the_rates_are_declared() {
        let msrs = TimeAbiMsrs::new();
        for msr in [MSR_TSC_FREQUENCY, MSR_APIC_FREQUENCY] {
            assert_eq!(
                serve_msr(&msrs, VpIndex::BSP, msr, MsrAccess::Read),
                Some(MsrOutcome::Fault)
            );
        }
    }

    fn sample(monotonic_ns: u64) -> HostTimeSample {
        HostTimeSample {
            utc_ns: 1_800_000_000_000_000_000 + monotonic_ns,
            monotonic_ns,
        }
    }

    #[test]
    fn anchors_pair_with_the_tightest_bracket_midpoint() {
        let candidates = [
            AnchorCandidate {
                tsc: 100,
                before: sample(1_000),
                after: sample(22_000),
            },
            AnchorCandidate {
                tsc: 200,
                before: sample(50_000),
                after: sample(59_001),
            },
            AnchorCandidate {
                tsc: 300,
                before: sample(90_000),
                after: sample(108_000),
            },
        ];
        let anchor = select_anchor(&candidates, MAX_ANCHOR_PAIRING_NS).unwrap();
        assert_eq!(anchor.tsc, 200);
        assert_eq!(anchor.sample, sample(54_500));
        assert_eq!(anchor.pairing_ns, 4_501);

        let error = select_anchor(&candidates[..1], 10_000).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::TscAnchor);
        assert!(error.message.contains("10500 ns"), "{error}");
        assert_eq!(
            select_anchor(&[], MAX_ANCHOR_PAIRING_NS).unwrap_err().code,
            TimeAbiCode::TscAnchor
        );
    }

    #[test]
    fn readback_requires_every_vp() {
        let readback: Vec<_> = (0..4).map(|vp| (VpIndex::new(vp), 0x1234)).collect();
        check_readback(0x1234, &readback).unwrap();
        let mut skewed = readback;
        skewed[2].1 = 0x1235;
        let error = check_readback(0x1234, &skewed).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::TscSyncReadback);
        assert!(error.message.contains("VP 2"), "{error}");
    }

    #[test]
    fn capabilities_exclude_hv1_and_the_kvm_clock() {
        let topology = TopologyBuilder::new_x86().build(1).unwrap();
        let config = time_abi_cpuid(1, true);
        let cpuid = partition_cpuid(own_leaves(), &config).unwrap();
        let x2apic = topology.apic_mode() != vm_topology::processor::x86::ApicMode::XApic;
        let mut lookup = |function, index| {
            let mut result = cpuid.result(function, index, &whp_cpuid(function, index));
            if function == 1 {
                // Match the topology's APIC mode and drop XSAVE, which the
                // fake host does not describe.
                result[2] &= !((1 << 21) | (1 << 26) | ECX1_OSXSAVE);
                if x2apic {
                    result[2] |= 1 << 21;
                }
            }
            result
        };
        let caps = virt::x86::X86PartitionCapabilities::from_cpuid(
            &topology,
            &mut virt::time_abi::identity::capabilities_cpuid(&mut lookup),
        )
        .unwrap();
        assert!(!caps.hv1);
        assert!(!caps.kvm_clock);
        assert!(!caps.tsc_deadline);
    }
}

/// Tests against WHP itself. Run them with `--ignored` on a host with the
/// Windows Hypervisor Platform.
#[cfg(test)]
mod whp_tests {
    use super::*;
    use virt::time_abi::identity::check_identity;
    use virt::time_abi::identity::check_time_bits;
    use virt::time_abi::identity::time_abi_cpuid;
    use virt::time_abi::rate::LAPIC_HZ_HYPERV;

    /// Returns a time ABI partition with `vp_count` VPs, configured as
    /// `VtlPartition::new` configures one, and its time ABI state.
    fn partition(vp_count: u32) -> (whp::Partition, WhpTimeAbi) {
        partition_with(vp_count, None)
    }

    /// Returns a time ABI partition with `vp_count` VPs whose features derive
    /// from `profile`, if any.
    fn partition_with(vp_count: u32, profile: Option<&CpuProfile>) -> (whp::Partition, WhpTimeAbi) {
        let config = TimeAbiConfig {
            cpuid: Arc::new(time_abi_cpuid(vp_count, true)),
            msrs: Arc::new(TimeAbiMsrs::new()),
            cpu_profile: String::new(),
        };
        let mut whp_config = whp::PartitionConfig::new().unwrap();
        whp_config
            .set_property(whp::PartitionProperty::ProcessorCount(vp_count))
            .unwrap();
        let mut exits = WHV_EXTENDED_VM_EXITS(0);
        let time_abi =
            WhpTimeAbi::configure(&config, profile, &mut whp_config, &mut exits).unwrap();
        whp_config
            .set_property(whp::PartitionProperty::LocalApicEmulationMode(
                whp::abi::WHvX64LocalApicEmulationModeXApic,
            ))
            .unwrap();
        whp_config
            .set_property(whp::PartitionProperty::ExtendedVmExits(exits))
            .unwrap();
        let partition = whp_config.create().unwrap();
        for vp in 0..vp_count {
            partition.create_vp(vp).create().unwrap();
        }
        (partition, time_abi)
    }

    fn native(
        partition: &whp::Partition,
        function: u32,
        index: u32,
    ) -> Result<[u32; 4], whp::WHvError> {
        let output = partition.vp(0).get_cpuid_output(function, index)?;
        Ok([output.Eax, output.Ebx, output.Ecx, output.Edx])
    }

    fn tsc(partition: &whp::Partition, vp: u32) -> u64 {
        partition.vp(vp).get_register(whp::Register64::Tsc).unwrap()
    }

    #[test]
    #[ignore = "requires WHP"]
    fn partition_meets_the_static_obligations() {
        let (partition, time_abi) = partition(4);
        let started = std::time::Instant::now();
        check_native_sentinels(|function| native(&partition, function, 0)).unwrap();
        check_feature_banks(|function, index| native(&partition, function, index)).unwrap();
        println!("native checks: {} us", started.elapsed().as_micros());
        // WHP answers the whole identity range past the exits, and every
        // hypervisor signature base, with zeros.
        let bases = (0x4000_0100..=0x4000_ff00).step_by(0x100);
        for function in (IDENTITY_MAX_LEAF + 1..=0x4000_00ff).chain(bases) {
            assert_eq!(
                native(&partition, function, 0).unwrap(),
                [0; 4],
                "{function:#x}"
            );
        }
        check_unscaled(
            partition.tsc_frequency().unwrap(),
            whp::capabilities::processor_clock_frequency().unwrap(),
        )
        .unwrap();
        assert_eq!(partition.apic_frequency().unwrap(), LAPIC_HZ_HYPERV);
        let exits = partition.extended_vm_exits().unwrap();
        assert!(exits.is_set(WHV_EXTENDED_VM_EXITS::X64CpuidExit));
        assert!(exits.is_set(WHV_EXTENDED_VM_EXITS::X64MsrExit));
        assert!(
            partition
                .x64_msr_exit_bitmap()
                .unwrap()
                .is_set(WHV_X64_MSR_EXIT_BITMAP::UnhandledMsrs)
        );

        let cpuid = time_abi.partition_cpuid(Vec::new()).unwrap();
        let record = effective_cpuid(&cpuid, |function, index| {
            native(&partition, function, index).map(|result| cpuid.result(function, index, &result))
        })
        .unwrap();
        let record = CpuidLeafSet::new(record);
        let mut lookup = |function, index| record.result(function, index, &[0; 4]);
        check_identity(&mut lookup, 4).unwrap();
        check_time_bits(&mut lookup, true).unwrap();
        println!(
            "tsc_hz {} bank1 {:#x} native 0x80000007 {:#x?}",
            partition.tsc_frequency().unwrap(),
            time_abi.features_bank1,
            native(&partition, 0x8000_0007, 0).unwrap()
        );
    }

    /// Configures a partition from this host's CPU profile, as partitions
    /// will once `TimeAbiConfig` carries the profile, and checks the static
    /// obligations and that the guest CPUID equals the profile's wherever the
    /// profile pins it.
    #[test]
    #[ignore = "requires WHP"]
    fn host_profile_configures_the_partition() {
        let profile = match cpu_profile::select_auto(&cpu_profile::HostCpuSignature::current()) {
            Ok(profile) => profile,
            Err(err) => {
                println!("skipped: no profile for this host: {err}");
                return;
            }
        };
        let (partition, time_abi) = partition_with(4, Some(profile));
        assert_eq!(time_abi.cpu_profile.as_deref(), Some(profile.id()));
        assert!(time_abi.features_xsave.is_some());
        check_native_sentinels(|function| native(&partition, function, 0)).unwrap();
        check_feature_banks(|function, index| native(&partition, function, index)).unwrap();

        let cpuid = time_abi.partition_cpuid(Vec::new()).unwrap();
        let record = effective_cpuid(&cpuid, |function, index| {
            native(&partition, function, index).map(|result| cpuid.result(function, index, &result))
        })
        .unwrap();
        let record = CpuidLeafSet::new(record);
        let mut lookup = |function, index| record.result(function, index, &[0; 4]);
        check_identity(&mut lookup, 4).unwrap();
        check_time_bits(&mut lookup, true).unwrap();

        // What the guest sees: the time ABI CPUID over WHP's results.
        let mut differences = Vec::new();
        for entry in profile.cpuid() {
            let (function, index) = entry.key();
            let index = index.unwrap_or(0);
            let native = native(&partition, function, index).unwrap();
            let guest = cpuid.result(function, index, &native);
            let pinned = entry.values().into_iter().zip(entry.masks());
            for (register, (actual, (value, mask))) in guest.into_iter().zip(pinned).enumerate() {
                let differs = (actual ^ value) & mask;
                if differs != 0 {
                    differences.push(format!(
                        "{function:#x}.{index} register {register}: guest {actual:#010x} profile {value:#010x} differing bits {differs:#010x}",
                    ));
                }
            }
        }
        println!(
            "{}: bank0 {:#x} bank1 {:#x} xsave {:#x?}",
            profile.id(),
            time_abi.features_bank0,
            time_abi.features_bank1,
            time_abi.features_xsave
        );
        assert!(differences.is_empty(), "{differences:#?}");
    }

    #[test]
    #[ignore = "requires WHP"]
    fn time_suspension_is_idempotent() {
        // The preflight probe and the synchronized set both suspend time, and
        // a fresh partition's time is frozen until a VP first runs.
        let (partition, _) = partition(2);
        partition.suspend_time().unwrap();
        partition.suspend_time().unwrap();
        let frozen = tsc(&partition, 0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(tsc(&partition, 0), frozen);
        partition.resume_time().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(tsc(&partition, 0) > frozen);
        println!("second resume: {:?}", partition.resume_time());
    }

    #[test]
    #[ignore = "requires WHP"]
    fn synchronized_set_is_exact_while_suspended() {
        for vp_count in [1, 2, 4, 8] {
            let (partition, _) = partition(vp_count);
            let hz = partition.tsc_frequency().unwrap();
            partition.resume_time().unwrap();
            for vp in 0..vp_count {
                partition
                    .vp(vp)
                    .set_register(whp::Register64::Tsc, 1_000_000 * u64::from(vp + 1))
                    .unwrap();
            }

            let started = std::time::Instant::now();
            partition.suspend_time().unwrap();
            let target = 0x1234_5678_9abc;
            for vp in 0..vp_count {
                partition
                    .vp(vp)
                    .set_register(whp::Register64::Tsc, target)
                    .unwrap();
            }
            let readback: Vec<_> = (0..vp_count)
                .map(|vp| (VpIndex::new(vp), tsc(&partition, vp)))
                .collect();
            check_readback(target, &readback).unwrap();
            let elapsed = started.elapsed();
            std::thread::sleep(std::time::Duration::from_millis(10));
            for vp in 0..vp_count {
                assert_eq!(tsc(&partition, vp), target, "VP {vp} moved while suspended");
            }
            partition.resume_time().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            // Time runs again without any VP run, from the target.
            let advanced = tsc(&partition, 0) - target;
            assert!(advanced >= hz / 100 && advanced < hz, "advanced {advanced}");
            println!(
                "{vp_count} VPs: suspend, write, and read back in {} us",
                elapsed.as_micros()
            );
        }
    }

    #[test]
    #[ignore = "requires WHP"]
    fn capture_anchor_pairs_within_the_bound() {
        let (partition, _) = partition(1);
        partition.resume_time().unwrap();
        let bsp = partition.vp(0);
        let candidates: Vec<_> = (0..ANCHOR_ATTEMPTS)
            .map(|_| {
                let before = sample_host_time().unwrap();
                let tsc = bsp.get_register(whp::Register64::Tsc).unwrap();
                let after = sample_host_time().unwrap();
                AnchorCandidate { tsc, before, after }
            })
            .collect();
        let mut widths: Vec<u64> = candidates.iter().map(|c| c.width_ns()).collect();
        widths.sort_unstable();
        println!("bracket widths (ns): {widths:?}");
        let anchor = select_anchor(&candidates, MAX_ANCHOR_PAIRING_NS).unwrap();
        println!("pairing {} ns", anchor.pairing_ns);
    }
}
