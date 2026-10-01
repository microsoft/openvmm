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
//! - **Processor features.** Both feature banks and the XSAVE features derive
//!   from the partition's CPU profile ([`profile_features`]), without the
//!   TSC-deadline timer, `IA32_TSC_ADJUST`, or APERF/MPERF. WHP's default
//!   banks omit speculation controls that the host offers (SPEC_CTRL, STIBP,
//!   and SSBD on Skylake-SP, PSFD on Ice Lake), so they are never used.
//! - **CPUID.** [`TimeAbiConfig::cpuid`] is the partition's complete
//!   effective CPUID. Every leaf of it outside the hypervisor range is a
//!   `CpuidResultList2` result, which WHP returns without an exit. Only the
//!   leaves WHP cannot present that way exit to OpenVMM: the leaves with
//!   per-VP APIC identity fields (1, `0xB`, `0x1F`, and `0x8000001E`), which
//!   the exit handler sets for each VP, and the identity leaves. The explicit
//!   zero leaves of the identity range do not exit: WHP answers the whole
//!   hypervisor range with zeros natively, which preflight samples. Each
//!   exit-list entry costs about 25 µs of partition setup, and each native
//!   CPUID read about 18 µs.
//! - **Invariant TSC.** The effective CPUID sets bit 8 of CPUID `0x80000007`
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
use crate::cpu_contract::CpuidTopology;
use crate::cpu_contract::fixup_vp_topology_cpuid;
use crate::profile_features::WhpFeatures;
use crate::profile_features::profile_features;
use crate::profile_features::supported_surface;
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
use virt::time_abi::surface::SupportedCpuSurface;
use virt::x86::topology::per_vp_cpuid_bits;
use whp::abi::WHV_CPUID_OUTPUT;
use whp::abi::WHV_EXTENDED_VM_EXITS;
use whp::abi::WHV_PROCESSOR_FEATURES;
use whp::abi::WHV_PROCESSOR_FEATURES1;
use whp::abi::WHV_PROCESSOR_XSAVE_FEATURES;
use whp::abi::WHV_X64_CPUID_RESULT2;
use whp::abi::WHV_X64_CPUID_RESULT2_FLAGS;
use whp::abi::WHV_X64_MSR_EXIT_BITMAP;
use whp::abi::WHvX64CpuidResult2FlagSubleafSpecific;
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
    /// The partition's complete effective CPUID, with the per-VP APIC
    /// identity bits unmasked.
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
    /// The XSAVE features.
    #[inspect(hex)]
    features_xsave: u64,
    /// The CPU profile the features derive from.
    cpu_profile: String,
    /// How many `CpuidResultList2` entries present the effective CPUID.
    cpuid_results: usize,
    /// The effective CPUID record, computed once: it depends only on the
    /// configuration, because it excludes guest state.
    #[inspect(skip)]
    effective: OnceLock<Vec<CpuidLeaf>>,
}

impl WhpTimeAbi {
    /// Programs a new WHP partition for the time ABI: the CPUID exits, the
    /// processor features, the CPUID results, and the unhandled-MSR exits.
    /// The partition must have passed [`validate_partition`].
    ///
    /// The processor feature banks and the XSAVE features derive from the
    /// pinned CPU profile `config.cpu_profile` ([`profile_features`]), and
    /// `config.cpuid`, the complete effective CPUID, becomes the CPUID results
    /// ([`cpuid_results`]) and the exits ([`cpuid_exits`]).
    pub(crate) fn configure(
        config: &TimeAbiConfig,
        whp_config: &mut whp::PartitionConfig,
        extended_exits: &mut WHV_EXTENDED_VM_EXITS,
    ) -> Result<Self, TimeAbiError> {
        let profile = pinned_profile(&config.cpu_profile)?;

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

        let cpuid_exits = cpuid_exits(&config.cpuid);
        whp_config
            .set_property(whp::PartitionProperty::CpuidExitList(&cpuid_exits))
            .map_err(|err| routing_error(format!("cannot set the CPUID exit list: {err}")))?;

        let available = whp::capabilities::processor_features().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("cannot query the processor features: {err}"),
            )
        })?;
        let available_xsave = whp::capabilities::processor_xsave_features().map_err(|err| {
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
        let features = processor_features(features);
        let xsave = derived.xsave;
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
        let cpuid_results = cpuid_results(&config.cpuid);
        whp_config
            .set_property(whp::PartitionProperty::CpuidResultList2(&cpuid_results))
            .map_err(|err| {
                TimeAbiError::new(
                    TimeAbiCode::CpuSurface,
                    format!(
                        "cannot program the {} CPUID results of the effective CPUID: {err}",
                        cpuid_results.len()
                    ),
                )
            })?;

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
            cpuid_results = cpuid_results.len(),
            cpu_profile = profile.id(),
            available_bank0 = format_args!("{:#x}", available.bank0.0),
            available_bank1 = format_args!("{:#x}", available.bank1.0),
            bank0 = format_args!("{:#x}", features.bank0.0),
            bank1 = format_args!("{:#x}", features.bank1.0),
            xsave = format_args!("{xsave:#x}"),
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
            cpu_profile: profile.id().to_owned(),
            cpuid_results: cpuid_results.len(),
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
        let cpuid = partition_cpuid(own, &self.cpuid, &self.cpuid_exits)?;
        check_cpuid_delivery(&cpuid, &self.cpuid_exits)?;
        Ok(cpuid)
    }
}

fn routing_error(message: impl Into<String>) -> TimeAbiError {
    TimeAbiError::new(TimeAbiCode::IdentityRouting, message)
}

/// Returns the host's CPUID as the root partition sees it: every leaf and
/// subleaf that [`cpu_profile::cpuid::enumerate`] walks. It describes the
/// processor that WHP virtualizes, for [`supported_surface`].
pub(crate) fn host_cpuid() -> Vec<cpu_profile::cpuid::CpuidEntry> {
    // The CPUID instruction exists only on x86-64 hosts, the only hosts of
    // x86-64 WHP partitions.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    let query = |leaf, subleaf| {
        let result = core::arch::x86_64::__cpuid_count(leaf, subleaf);
        Ok::<_, std::convert::Infallible>([result.eax, result.ebx, result.ecx, result.edx])
    };
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(target_arch = "x86_64"))]
    let query = |_, _| Ok::<_, std::convert::Infallible>([0; 4]);
    cpu_profile::cpuid::enumerate(query).unwrap_or_else(|never| match never {})
}

/// Returns the pinned CPU profile that `id` names. Core selects and verifies
/// the profile before the partition exists, so an unknown ID is an internal
/// error (`E_PROFILE_UNKNOWN`).
fn pinned_profile(id: &str) -> Result<&'static CpuProfile, TimeAbiError> {
    cpu_profile::pinned(id).ok_or_else(|| {
        TimeAbiError::new(
            TimeAbiCode::ProfileUnknown,
            format!("CPU profile {id:?} is not pinned in this OpenVMM"),
        )
    })
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

/// Returns whether WHP cannot present `function` through `CpuidResultList2`
/// alone, so that it must exit to OpenVMM: a leaf with per-VP APIC identity
/// fields, which the exit handler sets for each VP, or a hypervisor-range
/// leaf, which WHP does not serve without synthetic features.
fn must_exit(function: u32) -> bool {
    per_vp_cpuid_bits(function) != [0; 4] || HYPERVISOR_CPUID_RANGE.contains(&function)
}

/// Returns the CPUID exit list of a time ABI partition whose effective CPUID
/// is `config`: every leaf of it that [`must_exit`], except the explicit zero
/// leaves, sorted and unique. The other leaves are `CpuidResultList2` results
/// ([`cpuid_results`]).
pub(crate) fn cpuid_exits(config: &CpuidLeafSet) -> Vec<u32> {
    let mut exits: Vec<u32> = config
        .leaves()
        .iter()
        .filter(|leaf| must_exit(leaf.function) && !is_zero_fill(leaf))
        .map(|leaf| leaf.function)
        .collect();
    exits.sort_unstable();
    exits.dedup();
    exits
}

/// Returns the `CpuidResultList2` results of a time ABI partition whose
/// effective CPUID is `config`: one per leaf and subleaf outside the
/// hypervisor range, with its mask. The per-VP APIC identity bits are
/// unmasked in `config`, so they keep WHP's value for each VP.
///
/// WHP returns these results for leaves without an exit, and as the default
/// result of a CPUID exit, on which OpenVMM applies `config` again.
pub(crate) fn cpuid_results(config: &CpuidLeafSet) -> Vec<WHV_X64_CPUID_RESULT2> {
    let output = |registers: [u32; 4]| WHV_CPUID_OUTPUT {
        Eax: registers[0],
        Ebx: registers[1],
        Ecx: registers[2],
        Edx: registers[3],
    };
    config
        .leaves()
        .iter()
        .filter(|leaf| !HYPERVISOR_CPUID_RANGE.contains(&leaf.function) && leaf.mask != [0; 4])
        .map(|leaf| WHV_X64_CPUID_RESULT2 {
            Function: leaf.function,
            Index: leaf.index.unwrap_or(0),
            VpIndex: 0,
            Flags: if leaf.index.is_some() {
                WHvX64CpuidResult2FlagSubleafSpecific
            } else {
                WHV_X64_CPUID_RESULT2_FLAGS(0)
            },
            Output: output(std::array::from_fn(|i| leaf.result[i] & leaf.mask[i])),
            Mask: output(leaf.mask),
        })
        .collect()
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

/// Returns the CPUID results that the exit handler applies on a time ABI
/// partition: `own`, the backend's other results, for the leaves in `exits`
/// outside the hypervisor range, then `config`, so that the effective CPUID
/// overrides every other source. A leaf that does not exit reaches the guest
/// from `config` alone, through `CpuidResultList2`.
///
/// Fails with `E_IDENTITY_ROUTING` if another result would shadow part of
/// `config`: [`CpuidLeafSet::result`] applies only the first matching leaf,
/// so a subleaf-less leaf would hide a subleaf of `config`.
pub(crate) fn partition_cpuid(
    own: Vec<CpuidLeaf>,
    config: &CpuidLeafSet,
    exits: &[u32],
) -> Result<CpuidLeafSet, TimeAbiError> {
    let mut leaves: Vec<CpuidLeaf> = own
        .into_iter()
        .filter(|leaf| {
            !HYPERVISOR_CPUID_RANGE.contains(&leaf.function)
                && exits.binary_search(&leaf.function).is_ok()
        })
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

/// Checks that every leaf of `cpuid` reaches the guest: a leaf in the
/// hypervisor range exits, except the explicit zero leaves, which WHP
/// answers natively; any other leaf exits or is a `CpuidResultList2` result
/// ([`cpuid_results`]). Fails with `E_IDENTITY_ROUTING`, naming the first
/// leaf that would not.
pub(crate) fn check_cpuid_delivery(
    cpuid: &CpuidLeafSet,
    exits: &[u32],
) -> Result<(), TimeAbiError> {
    if let Some(leaf) = cpuid.leaves().iter().find(|leaf| {
        HYPERVISOR_CPUID_RANGE.contains(&leaf.function)
            && !is_zero_fill(leaf)
            && exits.binary_search(&leaf.function).is_err()
    }) {
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
/// for the maximum basic and extended leaves and for every leaf of the
/// partition's CPUID results `cpuid` (the effective CPUID, and the backend's
/// own results for the leaves that exit), with guest-state bits cleared. The
/// explicit zero leaves are reported as programmed; preflight samples them
/// natively.
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

/// The CPUID that one VP of a time ABI partition observes.
pub(crate) struct VpCpuid<'a> {
    /// The partition's CPUID results, which the exit handler applies.
    pub cpuid: &'a CpuidLeafSet,
    /// The CPUID exit list.
    pub exits: &'a [u32],
    /// The topology behind the per-VP fields.
    pub topology: &'a CpuidTopology,
    /// The VP's APIC ID.
    pub apic_id: u32,
}

impl VpCpuid<'_> {
    /// Returns the result the VP sees for `function` and `index`, given
    /// `native`, which reads WHP's result (including its `CpuidResultList2`
    /// result). A leaf that exits gets the partition's CPUID results over
    /// WHP's result, with the VP's APIC identity, as the exit handler
    /// computes them; any other leaf gets WHP's result. WHP's result is read
    /// only if a bit of it shows through.
    pub fn result<E>(
        &self,
        function: u32,
        index: u32,
        native: impl FnOnce() -> Result<[u32; 4], E>,
    ) -> Result<[u32; 4], E> {
        if self.exits.binary_search(&function).is_err() {
            return native();
        }
        let programmed = self
            .cpuid
            .leaves()
            .iter()
            .find(|leaf| leaf.matches(function, index));
        let base = match programmed {
            Some(leaf) if leaf.mask == [!0; 4] => [0; 4],
            _ => native()?,
        };
        let mut result = self.cpuid.result(function, index, &base);
        fixup_vp_topology_cpuid(self.topology, self.apic_id, function, index, &mut result);
        Ok(result)
    }
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
        let vp0 = VpCpuid {
            cpuid: &self.cpuid,
            exits: &state.cpuid_exits,
            topology: &self.cpuid_topology,
            apic_id: self.vps.first().map_or(0, |vp| vp.vp_info.apic_id),
        };
        let record = effective_cpuid(&self.cpuid, |function, index| {
            vp0.result(function, index, || native.get(function, index))
                .map_err(|err| {
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

    fn supported_cpu_surface(&self) -> Result<Option<SupportedCpuSurface>, TimeAbiError> {
        let started = std::time::Instant::now();
        let unsupported = |what: &str, err: whp::WHvError| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("cannot query {what}: {err}"),
            )
        };
        let banks = whp::capabilities::processor_features()
            .map_err(|err| unsupported("the processor features", err))?;
        let xsave = whp::capabilities::processor_xsave_features()
            .map_err(|err| unsupported("the XSAVE features", err))?;
        let physical_address_width = self
            .vtl0
            .whp
            .physical_address_width()
            .map_err(|err| unsupported("the physical address width", err))?;
        let surface = supported_surface(
            &host_cpuid(),
            WhpFeatures {
                banks: [banks.bank0.0, banks.bank1.0],
                xsave: xsave.0,
            },
            physical_address_width.try_into().unwrap_or(u8::MAX),
        );
        tracing::info!(
            cpuid = surface.cpuid.len(),
            physical_address_width = surface.physical_address_width,
            elapsed_us = started.elapsed().as_micros() as u64,
            "time ABI: WHP supported CPU surface"
        );
        Ok(Some(surface))
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

/// Test support: the complete effective CPUID that core passes in
/// [`TimeAbiConfig::cpuid`], built as core builds it, and core's check of the
/// CPUID that VP 0 presents.
#[cfg(test)]
pub(crate) mod test_cpuid {
    use cpu_profile::CpuProfile;
    use cpu_profile::EffectiveCpuid;
    use virt::CpuidLeaf;
    use virt::CpuidLeafSet;
    use vm_topology::processor::ProcessorTopology;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::ApicMode;
    use vm_topology::processor::x86::X2ApicState;

    /// The pinned CPU profiles.
    pub const PROFILES: [&str; 3] = [
        "intel.skylake-sp.v1",
        "intel.icelake-sp.v1",
        "intel.emeraldrapids.v1",
    ];

    /// Returns the topology of a VM with `vp_count` VPs in one socket.
    pub fn topology(vp_count: u32, x2apic: X2ApicState) -> ProcessorTopology {
        TopologyBuilder::new_x86()
            .vps_per_socket(vp_count)
            .x2apic(x2apic)
            .build(vp_count)
            .unwrap()
    }

    /// Returns the effective CPUID of a partition with `profile` and
    /// `topology`, as core's `time_abi::effective_cpuid` builds it.
    pub fn effective_cpuid(profile: &CpuProfile, topology: &ProcessorTopology) -> EffectiveCpuid {
        let result = |leaf: &CpuidLeaf| cpu_profile::CpuidResult {
            function: leaf.function,
            index: leaf.index,
            result: leaf.result,
            mask: leaf.mask,
        };
        let mut topology_leaves = Vec::new();
        virt::x86::topology::topology_cpuid(
            topology,
            &|leaf, subleaf| profile.lookup(leaf, subleaf),
            &mut topology_leaves,
        )
        .unwrap();
        let mut vm: Vec<_> = topology_leaves.iter().map(result).collect();
        vm.push(cpu_profile::x2apic_cpuid(!matches!(
            topology.apic_mode(),
            ApicMode::XApic
        )));
        let identity: Vec<_> = virt::time_abi::identity::identity_cpuid_leaves(topology.vp_count())
            .iter()
            .map(result)
            .chain(virt::time_abi::identity::identity_zero_cpuid_leaves().map(|leaf| result(&leaf)))
            .collect();
        profile.effective_cpuid(&vm, &identity).unwrap()
    }

    /// Returns [`TimeAbiConfig::cpuid`](virt::time_abi::TimeAbiConfig::cpuid)
    /// for `effective`, as core's `backend_cpuid` builds it: the per-VP APIC
    /// identity bits unmasked.
    pub fn config_cpuid(effective: &EffectiveCpuid) -> CpuidLeafSet {
        CpuidLeafSet::new(
            effective
                .results()
                .map(|result| {
                    let per_vp = virt::x86::topology::per_vp_cpuid_bits(result.function);
                    CpuidLeaf {
                        function: result.function,
                        index: result.index,
                        result: result.result,
                        mask: [0, 1, 2, 3]
                            .map(|register| result.mask[register] & !per_vp[register]),
                    }
                })
                .collect(),
        )
    }

    /// Lists every result of `effective` that `presented` does not match
    /// under the result's masks, as core's `check_presented_cpuid` does.
    pub fn differences(presented: &CpuidLeafSet, effective: &EffectiveCpuid) -> Vec<String> {
        effective
            .results()
            .filter_map(|expected| {
                let actual =
                    presented.result(expected.function, expected.index.unwrap_or(0), &[0; 4]);
                let registers: Vec<String> = (0..4)
                    .filter(|&register| {
                        (actual[register] ^ expected.result[register]) & expected.mask[register]
                            != 0
                    })
                    .map(|register| {
                        format!(
                            "register {register} is {:#010x}, not {:#010x} under mask {:#010x}",
                            actual[register], expected.result[register], expected.mask[register]
                        )
                    })
                    .collect();
                (!registers.is_empty()).then(|| {
                    format!(
                        "CPUID {:#x}/{:?}: {}",
                        expected.function,
                        expected.index,
                        registers.join(", ")
                    )
                })
            })
            .collect()
    }

    /// Returns the backend's own CPUID results, as the partition composes
    /// them before [`super::WhpTimeAbi::partition_cpuid`]: the x2APIC bit and
    /// OpenVMM's topology leaves, computed over `native`.
    pub fn own_cpuid(
        topology: &ProcessorTopology,
        native: &dyn Fn(u32, u32) -> [u32; 4],
    ) -> Vec<CpuidLeaf> {
        let mask = [0, 0, 1 << 21, 0];
        let value = match topology.apic_mode() {
            ApicMode::XApic => [0; 4],
            ApicMode::X2ApicSupported | ApicMode::X2ApicEnabled => mask,
        };
        let mut own = vec![CpuidLeaf::new(1, value).masked(mask)];
        virt::x86::topology::topology_cpuid(topology, native, &mut own).unwrap();
        own
    }
}

#[cfg(test)]
mod tests {
    use super::test_cpuid;
    use super::*;
    use virt::time_abi::DeclaredRates;
    use virt::time_abi::msr::MSR_APIC_FREQUENCY;
    use virt::time_abi::msr::MSR_TSC_FREQUENCY;
    use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
    use virt::time_abi::msr::MSR_VP_INDEX;
    use vm_topology::processor::x86::X2ApicState;

    const VP_COUNT: u32 = 8;

    #[test]
    fn cpu_profile_ids_resolve_to_pinned_profiles() {
        let skylake = pinned_profile("intel.skylake-sp.v1").unwrap();
        assert_eq!(skylake.id(), "intel.skylake-sp.v1");
        for id in ["interim.host.whp.v1", "intel.skylake-sp.whp.v1", ""] {
            assert_eq!(
                pinned_profile(id).unwrap_err().code,
                TimeAbiCode::ProfileUnknown,
                "{id:?}"
            );
        }
    }

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

    /// A simulated WHP partition: noise in every bit of every leaf, with the
    /// `CpuidResultList2` results applied, as the CPUID output and the
    /// default result of a CPUID exit report it. Only programmed bits can
    /// match the effective CPUID.
    struct FakeWhp {
        results: Vec<WHV_X64_CPUID_RESULT2>,
    }

    impl FakeWhp {
        fn new(config: &CpuidLeafSet) -> Self {
            Self {
                results: cpuid_results(config),
            }
        }

        fn native(&self, function: u32, index: u32) -> [u32; 4] {
            let seed = function.wrapping_mul(0x9e37_79b9) ^ index.wrapping_mul(0x85eb_ca6b);
            let mut result = [
                seed,
                seed.rotate_left(8) ^ 0xa5a5_a5a5,
                seed.rotate_left(16) ^ 0x5a5a_5a5a,
                !seed,
            ];
            for programmed in self.results.iter().filter(|result| {
                result.Function == function
                    && (!result.Flags.is_set(WHvX64CpuidResult2FlagSubleafSpecific)
                        || result.Index == index)
            }) {
                let output = programmed.Output;
                let mask = programmed.Mask;
                let output = [output.Eax, output.Ebx, output.Ecx, output.Edx];
                let mask = [mask.Eax, mask.Ebx, mask.Ecx, mask.Edx];
                for register in 0..4 {
                    result[register] =
                        (result[register] & !mask[register]) | (output[register] & mask[register]);
                }
            }
            result
        }
    }

    /// A time ABI partition's CPUID, as `configure` and the partition build
    /// compose it for `profile` and `vp_count` VPs.
    struct Partition {
        topology: vm_topology::processor::ProcessorTopology,
        effective: cpu_profile::EffectiveCpuid,
        config: CpuidLeafSet,
        exits: Vec<u32>,
        whp: FakeWhp,
        cpuid: CpuidLeafSet,
        cpuid_topology: CpuidTopology,
    }

    impl Partition {
        fn new(profile: &str, vp_count: u32, x2apic: X2ApicState) -> Self {
            let topology = test_cpuid::topology(vp_count, x2apic);
            let effective =
                test_cpuid::effective_cpuid(cpu_profile::pinned(profile).unwrap(), &topology);
            let config = test_cpuid::config_cpuid(&effective);
            let exits = cpuid_exits(&config);
            let whp = FakeWhp::new(&config);
            let own =
                test_cpuid::own_cpuid(&topology, &|function, index| whp.native(function, index));
            let cpuid = partition_cpuid(own, &config, &exits).unwrap();
            check_cpuid_delivery(&cpuid, &exits).unwrap();
            let cpuid_topology = CpuidTopology::new(&topology);
            Self {
                topology,
                effective,
                config,
                exits,
                whp,
                cpuid,
                cpuid_topology,
            }
        }

        fn vp(&self, apic_id: u32) -> VpCpuid<'_> {
            VpCpuid {
                cpuid: &self.cpuid,
                exits: &self.exits,
                topology: &self.cpuid_topology,
                apic_id,
            }
        }

        /// The CPUID that the VP with `apic_id` observes.
        fn observe(&self, apic_id: u32, function: u32, index: u32) -> [u32; 4] {
            self.vp(apic_id)
                .result(function, index, || {
                    Ok::<_, std::convert::Infallible>(self.whp.native(function, index))
                })
                .unwrap()
        }

        /// VP 0's effective CPUID record, as the backend reports it.
        fn record(&self) -> Vec<CpuidLeaf> {
            effective_cpuid(&self.cpuid, |function, index| {
                Ok::<_, std::convert::Infallible>(self.observe(0, function, index))
            })
            .unwrap()
        }
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
    fn exit_list_covers_the_per_vp_and_identity_leaves() {
        for profile in test_cpuid::PROFILES {
            let partition = Partition::new(profile, VP_COUNT, X2ApicState::Supported);
            let mut expected = vec![1, 0xb];
            if partition
                .config
                .leaves()
                .iter()
                .any(|leaf| leaf.function == 0x1f)
            {
                expected.push(0x1f);
            }
            expected.extend(0x4000_0000..=0x4000_0005);
            // WHP answers the explicit zero leaves natively, and every other
            // leaf through CpuidResultList2.
            assert_eq!(partition.exits, expected, "{profile}");
        }
    }

    #[test]
    fn cpuid_results_present_every_leaf_outside_the_hypervisor_range() {
        for profile in test_cpuid::PROFILES {
            let partition = Partition::new(profile, VP_COUNT, X2ApicState::Supported);
            let results = &partition.whp.results;
            let leaves: Vec<_> = partition
                .config
                .leaves()
                .iter()
                .filter(|leaf| {
                    !HYPERVISOR_CPUID_RANGE.contains(&leaf.function) && leaf.mask != [0; 4]
                })
                .collect();
            assert_eq!(results.len(), leaves.len(), "{profile}");
            for (result, leaf) in results.iter().zip(leaves) {
                let output = [
                    result.Output.Eax,
                    result.Output.Ebx,
                    result.Output.Ecx,
                    result.Output.Edx,
                ];
                let mask = [
                    result.Mask.Eax,
                    result.Mask.Ebx,
                    result.Mask.Ecx,
                    result.Mask.Edx,
                ];
                assert_eq!(
                    (
                        result.Function,
                        result.Index,
                        result.Flags.is_set(WHvX64CpuidResult2FlagSubleafSpecific)
                    ),
                    (leaf.function, leaf.index.unwrap_or(0), leaf.index.is_some()),
                    "{profile}"
                );
                assert_eq!(mask, leaf.mask, "{profile} {:#x}", leaf.function);
                assert_eq!(
                    output,
                    [0, 1, 2, 3].map(|register| leaf.result[register] & leaf.mask[register]),
                    "{profile} {:#x}",
                    leaf.function
                );
            }
            // The per-VP APIC identity and the runtime state (OSXSAVE) keep
            // WHP's value for each VP.
            let leaf = |function, index| {
                results
                    .iter()
                    .find(|result| result.Function == function && result.Index == index)
                    .unwrap()
            };
            assert_eq!(leaf(1, 0).Mask.Ebx & 0xff00_0000, 0, "{profile}");
            assert_eq!(leaf(1, 0).Mask.Ecx & ECX1_OSXSAVE, 0, "{profile}");
            assert_eq!(leaf(0xb, 0).Mask.Edx, 0, "{profile}");
            // The brand string is the profile's generic one, not the host's.
            let brand: String = [0x8000_0002, 0x8000_0003, 0x8000_0004]
                .iter()
                .flat_map(|&function| {
                    let result = leaf(function, 0);
                    [
                        result.Output.Eax,
                        result.Output.Ebx,
                        result.Output.Ecx,
                        result.Output.Edx,
                    ]
                })
                .flat_map(u32::to_le_bytes)
                .take_while(|&byte| byte != 0)
                .map(char::from)
                .collect();
            assert!(
                brand.starts_with("Intel(R) Xeon(R) Processor ("),
                "{profile}: {brand}"
            );
        }
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

    /// The check core runs before any VP runs (`E_CPU_SURFACE`): VP 0's
    /// record equals the effective CPUID under its masks, on every pinned
    /// profile, VP count, and APIC mode.
    #[test]
    fn vp0_presents_the_effective_cpuid() {
        for profile in test_cpuid::PROFILES {
            for vp_count in [1, 2, 4, 8, 64] {
                for x2apic in [
                    X2ApicState::Supported,
                    X2ApicState::Enabled,
                    X2ApicState::Unsupported,
                ] {
                    if x2apic == X2ApicState::Unsupported && vp_count > 8 {
                        continue;
                    }
                    let partition = Partition::new(profile, vp_count, x2apic);
                    let record = CpuidLeafSet::new(partition.record());
                    let differences = test_cpuid::differences(&record, &partition.effective);
                    assert!(
                        differences.is_empty(),
                        "{profile}, {vp_count} VPs, {x2apic:?}: {differences:#?}"
                    );
                    let mut lookup = |function, index| record.result(function, index, &[0; 4]);
                    virt::time_abi::identity::check_identity(&mut lookup, vp_count).unwrap();
                    virt::time_abi::identity::check_time_bits(&mut lookup, true).unwrap();
                }
            }
        }
    }

    /// Every VP observes the effective CPUID with its own APIC identity in
    /// the per-VP fields.
    #[test]
    fn each_vp_observes_its_own_apic_identity() {
        for profile in test_cpuid::PROFILES {
            let partition = Partition::new(profile, VP_COUNT, X2ApicState::Supported);
            for vp in partition.topology.vps_arch() {
                let apic_id = vp.apic_id;
                assert_eq!(
                    partition.observe(apic_id, 1, 0)[1] >> 24,
                    apic_id,
                    "{profile}"
                );
                for index in [0, 1] {
                    assert_eq!(partition.observe(apic_id, 0xb, index)[3], apic_id);
                }
                for leaf in partition.config.leaves() {
                    let index = leaf.index.unwrap_or(0);
                    let mut theirs = partition.observe(apic_id, leaf.function, index);
                    let mut ours = partition.observe(0, leaf.function, index);
                    let per_vp = per_vp_cpuid_bits(leaf.function);
                    for register in 0..4 {
                        theirs[register] &= !per_vp[register];
                        ours[register] &= !per_vp[register];
                    }
                    assert_eq!(theirs, ours, "{profile} VP {apic_id} {:#x}", leaf.function);
                }
            }
        }
    }

    #[test]
    fn partition_cpuid_keeps_own_results_only_for_exiting_leaves() {
        let partition = Partition::new(test_cpuid::PROFILES[0], VP_COUNT, X2ApicState::Supported);
        let own = vec![
            CpuidLeaf::new(4, [0x1c00_4121, 0, 0, 0]).indexed(0),
            CpuidLeaf::new(0x4000_0100, [0x4000_0101, 0x4b4d_564b, 0x564b_4d56, 0x4d]),
            CpuidLeaf::new(1, [0, 0x0008_0000, 0, 0]).masked([0, 0x00ff_0000, 0, 0]),
        ];
        let cpuid = partition_cpuid(own, &partition.config, &partition.exits).unwrap();
        for leaf in cpuid.leaves() {
            let programmed = partition
                .config
                .leaves()
                .iter()
                .find(|c| (c.function, c.index) == (leaf.function, leaf.index));
            match programmed {
                // Leaves that do not exit come from the effective CPUID alone.
                Some(c) if partition.exits.binary_search(&c.function).is_err() => {
                    assert_eq!(key(leaf), key(c), "{:#x}", leaf.function);
                }
                Some(_) => {}
                None => assert!(
                    partition.exits.binary_search(&leaf.function).is_ok(),
                    "{:#x}",
                    leaf.function
                ),
            }
        }
        assert!(
            !cpuid
                .leaves()
                .iter()
                .any(|leaf| leaf.function == 0x4000_0100)
        );
    }

    #[test]
    fn partition_cpuid_rejects_shadowed_subleaves() {
        let partition = Partition::new(test_cpuid::PROFILES[0], VP_COUNT, X2ApicState::Supported);
        // A subleaf-less leaf 0xB would hide the effective CPUID's 0xB.0.
        let own = vec![CpuidLeaf::new(0xb, [0; 4]).masked([0, !0, 0, 0])];
        assert_eq!(
            partition_cpuid(own, &partition.config, &partition.exits)
                .unwrap_err()
                .code,
            TimeAbiCode::IdentityRouting
        );
    }

    #[test]
    fn hypervisor_leaves_must_exit() {
        let partition = Partition::new(test_cpuid::PROFILES[0], VP_COUNT, X2ApicState::Supported);
        let without: Vec<u32> = partition
            .exits
            .iter()
            .copied()
            .filter(|&leaf| leaf != 0x4000_0003)
            .collect();
        let error = check_cpuid_delivery(&partition.cpuid, &without).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::IdentityRouting);
        assert!(error.message.contains("0x40000003"), "{error}");
        // A leaf outside the hypervisor range reaches the guest through
        // CpuidResultList2 without an exit.
        let only_identity: Vec<u32> = partition
            .exits
            .iter()
            .copied()
            .filter(|leaf| HYPERVISOR_CPUID_RANGE.contains(leaf))
            .collect();
        check_cpuid_delivery(&partition.config, &only_identity).unwrap();
    }

    #[test]
    fn effective_cpuid_ignores_guest_cr4_state() {
        let partition = Partition::new(test_cpuid::PROFILES[1], VP_COUNT, X2ApicState::Supported);
        let record = |cr4_bits: bool| {
            effective_cpuid(&partition.cpuid, |function, index| {
                let mut result = partition.observe(0, function, index);
                let (register, bit) = match (function, index) {
                    (1, _) => (2, ECX1_OSXSAVE),
                    (7, 0) => (2, ECX7_OSPKE),
                    _ => (0, 0),
                };
                if cr4_bits {
                    result[register] |= bit;
                } else {
                    result[register] &= !bit;
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
        let partition = Partition::new(test_cpuid::PROFILES[0], VP_COUNT, X2ApicState::Supported);
        let error = effective_cpuid(&partition.cpuid, |function, _| {
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
        let config =
            Partition::new(test_cpuid::PROFILES[0], VP_COUNT, X2ApicState::Supported).config;
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
        for profile in test_cpuid::PROFILES {
            let partition = Partition::new(profile, 1, X2ApicState::Supported);
            let mut lookup = |function, index| partition.observe(0, function, index);
            let caps = virt::x86::X86PartitionCapabilities::from_cpuid(
                &partition.topology,
                &mut virt::time_abi::identity::capabilities_cpuid(&mut lookup),
            )
            .unwrap();
            assert!(!caps.hv1, "{profile}");
            assert!(!caps.kvm_clock, "{profile}");
            assert!(!caps.tsc_deadline, "{profile}");
        }
    }
}

/// Tests against WHP itself. Run them with `--ignored` on a host with the
/// Windows Hypervisor Platform.
#[cfg(test)]
mod whp_tests {
    use super::test_cpuid;
    use super::*;
    use virt::time_abi::identity::check_identity;
    use virt::time_abi::identity::check_time_bits;
    use virt::time_abi::rate::LAPIC_HZ_HYPERV;
    use vm_topology::processor::x86::X2ApicState;

    /// A time ABI partition, its time ABI state, and its effective CPUID.
    struct Partition {
        partition: whp::Partition,
        time_abi: WhpTimeAbi,
        topology: vm_topology::processor::ProcessorTopology,
        effective: cpu_profile::EffectiveCpuid,
    }

    /// Returns a time ABI partition with `vp_count` VPs and this host's CPU
    /// profile, configured as `VtlPartition::new` configures one, or `None`
    /// on a host outside the pinned generations, where the tests skip.
    fn partition(vp_count: u32) -> Option<Partition> {
        let profile = match cpu_profile::select_auto(&cpu_profile::HostCpuSignature::current()) {
            Ok(profile) => profile,
            Err(err) => {
                println!("skipped: no profile for this host: {err}");
                return None;
            }
        };
        let topology = test_cpuid::topology(vp_count, X2ApicState::Supported);
        let effective = test_cpuid::effective_cpuid(profile, &topology);
        let config = TimeAbiConfig {
            cpuid: Arc::new(test_cpuid::config_cpuid(&effective)),
            msrs: Arc::new(TimeAbiMsrs::new()),
            cpu_profile: profile.id().to_owned(),
        };
        let mut whp_config = whp::PartitionConfig::new().unwrap();
        whp_config
            .set_property(whp::PartitionProperty::ProcessorCount(vp_count))
            .unwrap();
        let mut exits = WHV_EXTENDED_VM_EXITS(0);
        let time_abi = WhpTimeAbi::configure(&config, &mut whp_config, &mut exits).unwrap();
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
        Some(Partition {
            partition,
            time_abi,
            topology,
            effective,
        })
    }

    fn native_vp(
        partition: &whp::Partition,
        vp: u32,
        function: u32,
        index: u32,
    ) -> Result<[u32; 4], whp::WHvError> {
        let output = partition.vp(vp).get_cpuid_output(function, index)?;
        Ok([output.Eax, output.Ebx, output.Ecx, output.Edx])
    }

    fn native(
        partition: &whp::Partition,
        function: u32,
        index: u32,
    ) -> Result<[u32; 4], whp::WHvError> {
        native_vp(partition, 0, function, index)
    }

    fn tsc(partition: &whp::Partition, vp: u32) -> u64 {
        partition.vp(vp).get_register(whp::Register64::Tsc).unwrap()
    }

    #[test]
    #[ignore = "requires WHP"]
    fn partition_meets_the_static_obligations() {
        let Some(Partition {
            partition,
            time_abi,
            ..
        }) = partition(4)
        else {
            return;
        };
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
        println!(
            "tsc_hz {} bank1 {:#x} native 0x80000007 {:#x?}",
            partition.tsc_frequency().unwrap(),
            time_abi.features_bank1,
            native(&partition, 0x8000_0007, 0).unwrap()
        );
    }

    /// Configures a partition from this host's CPU profile and runs the
    /// check core runs before any VP runs (`E_CPU_SURFACE`) on every VP: the
    /// CPUID it observes, through `CpuidResultList2` or the exit handler,
    /// equals the effective CPUID under its masks, with its own APIC
    /// identity in the per-VP fields.
    #[test]
    #[ignore = "requires WHP"]
    fn host_profile_configures_the_partition() {
        const VP_COUNT: u32 = 4;
        let Some(Partition {
            partition,
            time_abi,
            topology,
            effective,
        }) = partition(VP_COUNT)
        else {
            return;
        };
        check_native_sentinels(|function| native(&partition, function, 0)).unwrap();
        check_feature_banks(|function, index| native(&partition, function, index)).unwrap();

        // The backend's own results, as the partition build composes them.
        let own = test_cpuid::own_cpuid(&topology, &|function, index| {
            native(&partition, function, index).unwrap()
        });
        let cpuid = time_abi.partition_cpuid(own).unwrap();
        let cpuid_topology = CpuidTopology::new(&topology);
        let started = std::time::Instant::now();
        let mut reads = 0;
        for vp in topology.vps_arch() {
            let index = vp.base.vp_index.index();
            let view = VpCpuid {
                cpuid: &cpuid,
                exits: &time_abi.cpuid_exits,
                topology: &cpuid_topology,
                apic_id: vp.apic_id,
            };
            let record = effective_cpuid(&cpuid, |function, subleaf| {
                view.result(function, subleaf, || {
                    reads += 1;
                    native_vp(&partition, index, function, subleaf)
                })
            })
            .unwrap();
            let record = CpuidLeafSet::new(record);
            if index == 0 {
                println!(
                    "VP 0 record: {} results, {reads} native reads in {} us",
                    record.leaves().len(),
                    started.elapsed().as_micros()
                );
                let mut lookup = |function, index| record.result(function, index, &[0; 4]);
                check_identity(&mut lookup, VP_COUNT).unwrap();
                check_time_bits(&mut lookup, true).unwrap();
                let differences = test_cpuid::differences(&record, &effective);
                assert!(differences.is_empty(), "VP 0: {differences:#?}");
            } else {
                // Each VP observes VP 0's CPUID outside the per-VP fields,
                // and its own APIC ID in them.
                assert_eq!(record.result(1, 0, &[0; 4])[1] >> 24, vp.apic_id);
                assert_eq!(record.result(0xb, 0, &[0; 4])[3], vp.apic_id);
                for leaf in effective.results() {
                    let subleaf = leaf.index.unwrap_or(0);
                    let per_vp = per_vp_cpuid_bits(leaf.function);
                    let actual = record.result(leaf.function, subleaf, &[0; 4]);
                    for register in 0..4 {
                        let mask = leaf.mask[register] & !per_vp[register];
                        assert_eq!(
                            (actual[register] ^ leaf.result[register]) & mask,
                            0,
                            "VP {index} CPUID {:#x}/{subleaf} register {register}",
                            leaf.function
                        );
                    }
                }
            }
        }
        println!(
            "{}: exits {:#x?} results {} bank0 {:#x} bank1 {:#x} xsave {:#x}",
            time_abi.cpu_profile,
            time_abi.cpuid_exits,
            time_abi.cpuid_results,
            time_abi.features_bank0,
            time_abi.features_bank1,
            time_abi.features_xsave
        );
    }

    #[test]
    #[ignore = "requires WHP"]
    fn time_suspension_is_idempotent() {
        // The preflight probe and the synchronized set both suspend time, and
        // a fresh partition's time is frozen until a VP first runs.
        let Some(Partition { partition, .. }) = partition(2) else {
            return;
        };
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
            let Some(Partition { partition, .. }) = partition(vp_count) else {
                return;
            };
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
        let Some(Partition { partition, .. }) = partition(1) else {
            return;
        };
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
