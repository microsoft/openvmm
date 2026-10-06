// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVX time ABI v1 on KVM.
//!
//! Active only for partitions built with
//! [`ProtoPartitionConfig::time_abi`](virt::ProtoPartitionConfig::time_abi).
//! It implements the KVM column of "Backend obligations" in NVX
//! `doc/design/time-abi.md`:
//!
//! - **CPUID and the CPU profile.** The partition's CPUID is the CPU
//!   profile's complete effective CPUID, programmed verbatim with
//!   `KVM_SET_CPUID2` (each vCPU gets its own APIC identity), so no KVM leaf
//!   remains and the guest sees the Hyper-V frequency identity instead of
//!   KVM's signature. Without a KVM signature the guest has no KVM
//!   paravirtual feature, and `KVM_CAP_ENFORCE_PV_FEATURE_CPUID` makes their
//!   MSRs (kvmclock and the others) raise #GP. Every vCPU presents the
//!   profile's `IA32_ARCH_CAPABILITIES` in the bits the profile pins, and
//!   KVM's own value in the others. KVM's supported CPUID and
//!   `IA32_ARCH_CAPABILITIES` feature MSR form the surface for the profile's
//!   support check. The partition capabilities come from the CPUID with the
//!   hypervisor range masked, so the partition does not model Hyper-V
//!   (`hv1`).
//! - **Identity MSRs.** An MSR filter denies `0x40000000..=0x400001ff`,
//!   `IA32_TSC_ADJUST`, `IA32_TSC_DEADLINE`, and the legacy P6 L2-cache MSRs,
//!   and `KVM_MSR_EXIT_REASON_FILTER` delivers every guest access to OpenVMM:
//!   [`TimeAbiMsrs`] serves the identity range, and the other MSRs raise #GP
//!   (with their CPUID bits clear, KVM would still serve `IA32_TSC_ADJUST`,
//!   and would read `IA32_TSC_DEADLINE` as 0 and ignore writes; KVM serves
//!   reads of `MSR_IA32_BBL_CR_CTL3`, and OpenVMM stubs the other L2-cache
//!   MSRs for the PCAT BIOS). KVM checks the filter before its in-kernel
//!   Hyper-V MSRs, so those never see a guest access. Under an AMD CPU
//!   profile, the filter also denies HWCR and DE_CFG, which OpenVMM serves
//!   with the time ABI's fixed values
//!   ([`AMD_CONFIG_MSRS`](virt::time_abi::msr::AMD_CONFIG_MSRS)) instead of
//!   KVM's own (HWCR reading 0).
//! - **Invariant TSC.** With `"Hv#1"` and `AccessTscInvariantControls` in
//!   CPUID, KVM (Linux 6.3 and later) hides the invariant-TSC bit of CPUID
//!   `0x80000007` until its own copy of `HV_X64_MSR_TSC_INVARIANT_CONTROL` is
//!   set. The filter keeps the guest's writes away from KVM, so the backend
//!   mirrors the guest-visible value into KVM with host-initiated writes,
//!   which bypass the filter: after every guest write, and before a VP runs
//!   after the value changed otherwise (restore and reset).
//! - **TSC.** The guest TSC is the host TSC plus a per-vCPU offset and is
//!   never scaled: the backend never calls `KVM_SET_TSC_KHZ`, and `F_d` is
//!   VP 0's `KVM_GET_TSC_KHZ`. The synchronized set writes one
//!   `KVM_VCPU_TSC_OFFSET` (Linux 5.16 and later) to every vCPU, computed from
//!   one host TSC read, and reads every offset back.
//! - **LAPIC.** The in-kernel LAPIC timer counts KVM's fixed 1 GHz bus clock;
//!   `KVM_CAP_X86_APIC_BUS_CYCLES_NS` is never set.
//!
//! **Restore and the saved per-VP TSC.** A time ABI restore omits every VP's
//! saved TSC (core strips the `tsc` element before VP state restore), so no
//! host-initiated `IA32_TSC` write reaches KVM's TSC synchronization
//! heuristic. That heuristic treats a write within about a second of the TSC
//! KVM expects as a synchronization attempt and keeps its own offset; Linux
//! 6.6 applies it to the first write after vCPU creation too, which would
//! discard the saved TSC of a snapshot taken within about a second of the
//! guest's boot. Each vCPU instead keeps its creation-time offset until the
//! synchronized set writes the common offset, before any VP runs, on every
//! kernel. KVM computes the LAPIC timer's TSC deadline when the LAPIC state is
//! set, so the restore orchestrator re-applies the LAPIC state after the
//! synchronized set.

use super::KvmPartitionInner;
use crate::KvmError;
use cpu_profile::CpuProfile;
use cpu_profile::CpuVendor;
use parking_lot::Mutex;
use std::ops::RangeInclusive;
use std::sync::Arc;
use virt::CpuidLeaf;
use virt::CpuidLeafSet;
use virt::VpIndex;
use virt::time_abi::BackendPreflight;
use virt::time_abi::HostTimeSample;
use virt::time_abi::IdentityMsrRoute;
use virt::time_abi::MAX_ANCHOR_PAIRING_NS;
use virt::time_abi::TimeAbiBackend;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiError;
use virt::time_abi::TimeAbiMsrs;
use virt::time_abi::TscAnchor;
use virt::time_abi::TscSetReport;
use virt::time_abi::TscSyncMethod;
use virt::time_abi::host::sample_host_time;
use virt::time_abi::msr::AMD_CONFIG_MSRS;
use virt::time_abi::msr::IDENTITY_MSR_RANGE;
use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
use virt::time_abi::msr::amd_config_msr_read;
use virt::time_abi::msr::amd_config_msr_write;
use virt::time_abi::rate::LAPIC_HZ_KVM;
use virt::time_abi::surface::SupportedCpuSurface;
use virt::time_abi::surface::SupportedMsrValue;
use virt::x86::MsrError;
use x86defs::cpuid::CpuidFunction;

/// `IA32_ARCH_CAPABILITIES`, which the CPU profile pins.
pub(crate) const MSR_IA32_ARCH_CAPABILITIES: u32 = 0x10a;

/// `IA32_TSC_ADJUST`, which the time ABI hides.
const MSR_IA32_TSC_ADJUST: u32 = 0x3b;

/// `IA32_TSC_DEADLINE`, which the time ABI hides.
const MSR_IA32_TSC_DEADLINE: u32 = 0x6e0;

/// The TSC MSRs that the time ABI hides; they raise #GP.
const HIDDEN_TSC_MSRS: [u32; 2] = [MSR_IA32_TSC_ADJUST, MSR_IA32_TSC_DEADLINE];

/// The legacy P6 L2-cache MSRs that the backend stubs for Windows booting
/// from the PCAT BIOS (`MYSTERY_MSRS` in the parent module). No CPU profile
/// pins them and a microVM never probes them, so they raise #GP on a time
/// ABI partition, as on MSHV. KVM itself serves reads of `0x11e`
/// (`MSR_IA32_BBL_CR_CTL3`), so the filter denies them all.
const LEGACY_L2_CACHE_MSRS: [RangeInclusive<u32>; 4] =
    [0x88..=0x8a, 0x116..=0x116, 0x118..=0x11b, 0x11e..=0x11e];

/// Whether a guest access to `msr` raises #GP on a time ABI partition,
/// outside the identity range.
fn is_hidden_msr(msr: u32) -> bool {
    HIDDEN_TSC_MSRS.contains(&msr)
        || LEGACY_L2_CACHE_MSRS
            .iter()
            .any(|range| range.contains(&msr))
}

/// Pairing attempts before an anchor fails: an attempt only misses
/// [`MAX_ANCHOR_PAIRING_NS`] when the thread is interrupted between the two
/// TSC reads around the host sample.
const ANCHOR_ATTEMPTS: usize = 16;

/// Returns the MSR filter ranges of the time ABI: deny every guest access to
/// the identity range, the hidden TSC MSRs, and the legacy L2-cache MSRs, so
/// they exit to OpenVMM.
pub(crate) fn identity_msr_filter() -> [kvm::MsrFilterRange; 7] {
    let [l2_a, l2_b, l2_c, l2_d] = LEGACY_L2_CACHE_MSRS;
    [
        kvm::MsrFilterRange::deny(IDENTITY_MSR_RANGE),
        kvm::MsrFilterRange::deny(MSR_IA32_TSC_ADJUST..=MSR_IA32_TSC_ADJUST),
        kvm::MsrFilterRange::deny(MSR_IA32_TSC_DEADLINE..=MSR_IA32_TSC_DEADLINE),
        kvm::MsrFilterRange::deny(l2_a),
        kvm::MsrFilterRange::deny(l2_b),
        kvm::MsrFilterRange::deny(l2_c),
        kvm::MsrFilterRange::deny(l2_d),
    ]
}

/// Returns the MSR filter ranges of a time ABI partition whose CPU profile is
/// of `vendor`: [`identity_msr_filter`], and for AMD also AMD's
/// configuration MSRs ([`AMD_CONFIG_MSRS`]), which KVM would otherwise
/// serve itself (HWCR reading 0, and DE_CFG reading what the VMM last set).
pub(crate) fn time_abi_msr_filter(vendor: CpuVendor) -> Vec<kvm::MsrFilterRange> {
    let mut ranges = identity_msr_filter().to_vec();
    if vendor == CpuVendor::Amd {
        ranges.extend(
            AMD_CONFIG_MSRS
                .iter()
                .map(|&(msr, _)| kvm::MsrFilterRange::deny(msr..=msr)),
        );
    }
    ranges
}

/// Routes the time ABI's MSRs to user space: user-space exits for unknown
/// and filtered MSRs, and the [`time_abi_msr_filter`] of a CPU profile of
/// `vendor` (every other MSR keeps its in-kernel handling).
pub(crate) fn install_identity_msr_filter(
    vm: &kvm::Partition,
    vendor: CpuVendor,
) -> Result<(), TimeAbiError> {
    for (name, cap) in [
        (
            "KVM_CAP_X86_USER_SPACE_MSR",
            kvm::KVM_CAP_X86_USER_SPACE_MSR,
        ),
        ("KVM_CAP_X86_MSR_FILTER", kvm::KVM_CAP_X86_MSR_FILTER),
    ] {
        if !vm.check_extension(cap).is_ok_and(|value| value > 0) {
            return Err(routing_error(format!(
                "KVM lacks {name} (Linux 5.10 or later)"
            )));
        }
    }
    vm.enable_msr_exits(kvm::KVM_MSR_EXIT_REASON_UNKNOWN | kvm::KVM_MSR_EXIT_REASON_FILTER)
        .map_err(|err| routing_error(format!("cannot enable user-space MSR exits: {err:#}")))?;
    vm.set_msr_filter(true, &time_abi_msr_filter(vendor))
        .map_err(|err| routing_error(format!("cannot install the MSR filter: {err:#}")))?;
    Ok(())
}

/// Returns the partition CPUID: the time ABI CPUID verbatim. It is the CPU
/// profile's complete effective CPUID (the profile's leaves, OpenVMM's
/// topology leaves and APIC mode, and the identity and explicit zero leaves),
/// so neither KVM's supported CPUID nor any other source adds a leaf or a
/// bit. `bind` sets each vCPU's own APIC identity in the per-VP fields.
pub(crate) fn partition_cpuid(time_abi: &CpuidLeafSet) -> CpuidLeafSet {
    CpuidLeafSet::new(time_abi.leaves().to_vec())
}

/// Returns the CPU surface KVM supports on this host, for the CPU profile's
/// support check (`E_PROFILE_UNSUPPORTED`): `supported` is
/// `KVM_GET_SUPPORTED_CPUID` as the partition queries it (without permission
/// for dynamically enabled XSAVE features), the physical address width is
/// the one it reports, and `arch_capabilities` is KVM's
/// `IA32_ARCH_CAPABILITIES` feature MSR, any bit of which the VMM may present
/// clear. AMD's PSFD is cleared where KVM advertises it without the
/// SPEC_CTRL access that a guest needs to use it (see `strip_psfd_leaf`), so
/// a profile that sets it fails the check instead of losing it silently.
pub(crate) fn supported_surface(
    supported: &[kvm::kvm_cpuid_entry2],
    arch_capabilities: u64,
) -> SupportedCpuSurface {
    let mut cpuid = cpuid_leaves(supported);
    if let Some(psfd_fixup) = super::strip_psfd_leaf(&CpuidLeafSet::new(cpuid.clone())) {
        for leaf in cpuid
            .iter_mut()
            .filter(|leaf| leaf.function == psfd_fixup.function)
        {
            psfd_fixup.apply(&mut leaf.result);
        }
    }
    let physical_address_width = cpuid
        .iter()
        .find(|leaf| leaf.function == CpuidFunction::ExtendedAddressSpaceSizes.0)
        .map_or(0, |leaf| leaf.result[0] as u8);
    SupportedCpuSurface {
        cpuid,
        physical_address_width,
        msrs: vec![SupportedMsrValue {
            index: MSR_IA32_ARCH_CAPABILITIES,
            supported: arch_capabilities,
            controllable: !0,
        }],
    }
}

/// Returns the `IA32_ARCH_CAPABILITIES` value that every vCPU presents under
/// `profile`: the profile's pinned bits, and KVM's own value (`supported`) in
/// the bits it leaves to the backend. `None` when the profile pins no value
/// or its CPUID (`cpuid`) does not enumerate the MSR, which then raises #GP
/// and keeps KVM's default.
pub(crate) fn profile_arch_capabilities(
    profile: &CpuProfile,
    supported: u64,
    cpuid: &CpuidLeafSet,
) -> Option<u64> {
    let (value, mask) = profile.msr(MSR_IA32_ARCH_CAPABILITIES)?;
    let leaf7_edx = cpuid.result(CpuidFunction::ExtendedFeatures.0, 0, &[0; 4])[3];
    let enumerated =
        x86defs::cpuid::ExtendedFeatureSubleaf0Edx::from(leaf7_edx).arch_capabilities();
    enumerated.then_some((value & mask) | (supported & !mask))
}

/// Converts the KVM CPUID entries programmed for a VP to CPUID leaves.
pub(crate) fn cpuid_leaves(entries: &[kvm::kvm_cpuid_entry2]) -> Vec<CpuidLeaf> {
    entries
        .iter()
        .map(|entry| {
            let leaf = CpuidLeaf::new(entry.function, [entry.eax, entry.ebx, entry.ecx, entry.edx]);
            if entry.flags & kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX != 0 {
                leaf.indexed(entry.index)
            } else {
                leaf
            }
        })
        .collect()
}

fn routing_error(message: impl Into<String>) -> TimeAbiError {
    TimeAbiError::new(TimeAbiCode::IdentityRouting, message)
}

/// Two host TSC reads around another host operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostTscBracket {
    pub start: u64,
    pub end: u64,
}

/// Waits until every earlier instruction has completed locally, so that a
/// TSC read is not reordered with its neighbors.
fn lfence() {
    // SAFETY: LFENCE only orders instruction execution.
    unsafe { core::arch::asm!("lfence", options(nostack, preserves_flags)) };
}

impl HostTscBracket {
    /// Runs `f` between two host TSC reads.
    pub(crate) fn around<T>(f: impl FnOnce() -> T) -> (Self, T) {
        lfence();
        let start = safe_intrinsics::rdtsc();
        lfence();
        let value = f();
        lfence();
        let end = safe_intrinsics::rdtsc();
        (Self { start, end }, value)
    }

    /// The host TSC at the middle of the bracket, and the most the instant of
    /// the bracketed operation can be from it, in cycles. `None` if the TSC
    /// went backwards.
    pub(crate) fn midpoint(&self) -> Option<(u64, u64)> {
        let width = self.end.checked_sub(self.start)?;
        Some((self.start + width / 2, width.div_ceil(2)))
    }
}

/// Converts `cycles` at `tsc_hz` to nanoseconds, rounding up.
fn cycles_to_ns_ceil(cycles: u64, tsc_hz: u64) -> u64 {
    let ns = (u128::from(cycles) * 1_000_000_000).div_ceil(u128::from(tsc_hz.max(1)));
    u64::try_from(ns).unwrap_or(u64::MAX)
}

/// Pairs a host time sample with the bracket of host TSC reads around it.
///
/// Returns the host TSC at the middle of the bracket and the pairing
/// uncertainty, half the bracket, in nanoseconds at `tsc_hz`. `None` if the
/// TSC went backwards.
pub(crate) fn pair_host_sample(bracket: HostTscBracket, tsc_hz: u64) -> Option<(u64, u64)> {
    let (host_tsc, half_width) = bracket.midpoint()?;
    Some((host_tsc, cycles_to_ns_ceil(half_width, tsc_hz)))
}

/// A host time sample paired with the host TSC.
#[derive(Debug, Clone, Copy)]
struct PairedSample {
    host_tsc: u64,
    sample: HostTimeSample,
    pairing_ns: u64,
}

/// Takes the `anchor` ("capture anchor" or "restore anchor"): a host time
/// sample paired with the host TSC within [`MAX_ANCHOR_PAIRING_NS`].
fn paired_host_sample(tsc_hz: u64, anchor: &str) -> Result<PairedSample, TimeAbiError> {
    pair_within_bound(tsc_hz, anchor, || HostTscBracket::around(sample_host_time))
}

/// Samples with `sample` until a pair is within [`MAX_ANCHOR_PAIRING_NS`],
/// at most [`ANCHOR_ATTEMPTS`] times, then fails with `E_TSC_ANCHOR`.
fn pair_within_bound(
    tsc_hz: u64,
    anchor: &str,
    mut sample: impl FnMut() -> (HostTscBracket, Result<HostTimeSample, TimeAbiError>),
) -> Result<PairedSample, TimeAbiError> {
    let mut tightest_ns: Option<u64> = None;
    for _ in 0..ANCHOR_ATTEMPTS {
        let (bracket, host) = sample();
        let host = host?;
        let Some((host_tsc, pairing_ns)) = pair_host_sample(bracket, tsc_hz) else {
            continue;
        };
        if pairing_ns <= MAX_ANCHOR_PAIRING_NS {
            return Ok(PairedSample {
                host_tsc,
                sample: host,
                pairing_ns,
            });
        }
        tightest_ns = Some(tightest_ns.map_or(pairing_ns, |ns| ns.min(pairing_ns)));
    }
    Err(TimeAbiError::new(
        TimeAbiCode::TscAnchor,
        format!(
            "cannot pair the {anchor} with a host time sample within {MAX_ANCHOR_PAIRING_NS} ns \
             in {ANCHOR_ATTEMPTS} attempts (tightest pairing: {})",
            tightest_ns.map_or("none, the host TSC went backwards".into(), |ns| format!(
                "{ns} ns"
            ))
        ),
    ))
}

/// Returns whether the guest TSC read from a vCPU between the host TSC reads
/// of `bracket` is the host TSC plus `offset`, as it is without TSC scaling.
pub(crate) fn tsc_is_unscaled(guest_tsc: u64, offset: u64, bracket: HostTscBracket) -> bool {
    let host_tsc = guest_tsc.wrapping_sub(offset);
    (bracket.start..=bracket.end).contains(&host_tsc)
}

/// Checks the synchronized set's read-back: every vCPU's offset must be the
/// common offset.
pub(crate) fn check_offset_readback(
    offset: u64,
    readback: &[(VpIndex, u64)],
) -> Result<(), TimeAbiError> {
    let mismatched: Vec<_> = readback
        .iter()
        .filter(|(_, value)| *value != offset)
        .map(|(vp, value)| format!("VP {}: {value:#x}", vp.index()))
        .collect();
    if !mismatched.is_empty() {
        return Err(TimeAbiError::new(
            TimeAbiCode::TscSyncReadback,
            format!(
                "KVM_VCPU_TSC_OFFSET read back differs from the common offset {offset:#x}: {}",
                mismatched.join(", ")
            ),
        ));
    }
    Ok(())
}

/// The time ABI state of a KVM partition.
#[derive(Debug)]
pub(crate) struct KvmTimeAbi {
    msrs: Arc<TimeAbiMsrs>,
    /// The value of KVM's own `HV_X64_MSR_TSC_INVARIANT_CONTROL`, or `None`
    /// if this KVM does not implement it (Linux before 6.3, or built without
    /// Hyper-V emulation), in which case KVM does not hide invariant TSC
    /// either.
    kvm_invariant_control: Option<Mutex<u64>>,
    /// The CPU surface KVM supports, taken when the partition was created.
    surface: SupportedCpuSurface,
    /// The `IA32_ARCH_CAPABILITIES` value of every vCPU, from the CPU
    /// profile ([`profile_arch_capabilities`]).
    arch_capabilities: Option<u64>,
    /// Whether the CPU profile is AMD's, so that OpenVMM serves AMD's
    /// configuration MSRs, which [`time_abi_msr_filter`] routes to it.
    amd_config_msrs: bool,
}

impl KvmTimeAbi {
    /// Sets up the vCPUs of a new partition for the time ABI: KVM's
    /// paravirtual MSRs raise #GP, and KVM's copy of
    /// `HV_X64_MSR_TSC_INVARIANT_CONTROL` is probed. `surface`,
    /// `arch_capabilities`, and `vendor` come from the partition's CPU
    /// profile.
    pub(crate) fn new(
        vm: &kvm::Partition,
        vcpus: &[u32],
        msrs: Arc<TimeAbiMsrs>,
        surface: SupportedCpuSurface,
        arch_capabilities: Option<u64>,
        vendor: CpuVendor,
    ) -> Result<Self, TimeAbiError> {
        for &vcpu in vcpus {
            vm.vp(vcpu)
                .enable_enforce_pv_feature_cpuid()
                .map_err(|err| {
                    routing_error(format!(
                        "cannot hide KVM's paravirtual MSRs on vCPU {vcpu}: {err:#}"
                    ))
                })?;
        }
        // Writing KVM's power-on value tells whether KVM implements the MSR.
        // `vcpus[0]` is VP 0.
        let kvm_invariant_control =
            match vm.vp(vcpus[0]).set_msrs(&[(MSR_TSC_INVARIANT_CONTROL, 0)]) {
                Ok(()) => Some(Mutex::new(0)),
                Err(kvm::Error::IncompleteMsrs { .. }) => {
                    tracing::info!(
                        "KVM lacks HV_X64_MSR_TSC_INVARIANT_CONTROL, so it does not hide \
                         invariant TSC"
                    );
                    None
                }
                Err(err) => {
                    return Err(routing_error(format!(
                        "cannot write KVM's HV_X64_MSR_TSC_INVARIANT_CONTROL: {err:#}"
                    )));
                }
            };
        Ok(Self {
            msrs,
            kvm_invariant_control,
            surface,
            arch_capabilities,
            amd_config_msrs: vendor == CpuVendor::Amd,
        })
    }

    /// Presents the CPU profile's `IA32_ARCH_CAPABILITIES` on `vp`, with a
    /// host-initiated write after its CPUID is set: KVM accepts the MSR only
    /// for a vCPU whose CPUID enumerates it, and keeps the value across
    /// resets.
    pub(crate) fn set_arch_capabilities(
        &self,
        vp: &kvm::Processor<'_>,
    ) -> Result<(), TimeAbiError> {
        let Some(value) = self.arch_capabilities else {
            return Ok(());
        };
        vp.set_msrs(&[(MSR_IA32_ARCH_CAPABILITIES, value)])
            .map_err(|err| {
                TimeAbiError::new(
                    TimeAbiCode::ProfileUnsupported,
                    format!(
                        "KVM cannot present the CPU profile's IA32_ARCH_CAPABILITIES {value:#x}: \
                         {err:#}"
                    ),
                )
            })
    }

    /// Handles a guest read of `msr` that exited to user space. `None` means
    /// the MSR is not the time ABI's.
    pub(crate) fn read_msr(&self, vp: VpIndex, msr: u32) -> Option<Result<u64, MsrError>> {
        if is_hidden_msr(msr) {
            tracelimit::info_ratelimited!(vp = vp.index(), msr, "hidden MSR read raises #GP");
            return Some(Err(MsrError::InvalidAccess));
        }
        if self.amd_config_msrs
            && let Some(value) = amd_config_msr_read(msr)
        {
            return Some(Ok(value));
        }
        self.msrs.read(vp, msr)
    }

    /// Handles a guest write of `value` to `msr` that exited to user space.
    /// `None` means the MSR is not the time ABI's.
    ///
    /// After an accepted `HV_X64_MSR_TSC_INVARIANT_CONTROL` write, the caller
    /// mirrors it with [`Self::sync_invariant_control`] before resuming the
    /// guest.
    pub(crate) fn write_msr(
        &self,
        vp: VpIndex,
        msr: u32,
        value: u64,
    ) -> Option<Result<(), MsrError>> {
        if is_hidden_msr(msr) {
            tracelimit::info_ratelimited!(
                vp = vp.index(),
                msr,
                value,
                "hidden MSR write raises #GP"
            );
            return Some(Err(MsrError::InvalidAccess));
        }
        if self.amd_config_msrs
            && let Some(result) = amd_config_msr_write(msr, value)
        {
            return Some(result);
        }
        self.msrs.write(vp, msr, value)
    }

    /// Makes KVM's copy of `HV_X64_MSR_TSC_INVARIANT_CONTROL` equal the
    /// guest-visible value, with a host-initiated write through `vp` when it
    /// differs.
    pub(crate) fn sync_invariant_control(
        &self,
        vp: &kvm::Processor<'_>,
    ) -> Result<(), TimeAbiError> {
        let Some(kvm_value) = &self.kvm_invariant_control else {
            return Ok(());
        };
        // Hold the lock while writing so that concurrent callers apply the
        // latest guest-visible value last.
        let mut kvm_value = kvm_value.lock();
        let value = self.msrs.tsc_invariant_control();
        if *kvm_value != value {
            vp.set_msrs(&[(MSR_TSC_INVARIANT_CONTROL, value)])
                .map_err(|err| {
                    routing_error(format!(
                        "cannot mirror HV_X64_MSR_TSC_INVARIANT_CONTROL = {value} into KVM: {err:#}"
                    ))
                })?;
            tracing::debug!(value, "mirrored HV_X64_MSR_TSC_INVARIANT_CONTROL into KVM");
            *kvm_value = value;
        }
        Ok(())
    }
}

impl KvmPartitionInner {
    fn time_abi_vps(&self) -> impl Iterator<Item = (VpIndex, kvm::Processor<'_>)> {
        self.vps.iter().map(|vp| {
            (
                vp.vp_info().base.vp_index,
                self.kvm.vp(vp.vp_info().apic_id),
            )
        })
    }
}

impl TimeAbiBackend for KvmPartitionInner {
    fn native_tsc_hz(&self) -> Result<u64, TimeAbiError> {
        self.vp_kvm(VpIndex::BSP).tsc_frequency_hz().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::TscRateUnavailable,
                format!("KVM_GET_TSC_KHZ failed on VP 0: {err:#}"),
            )
        })
    }

    fn lapic_hz(&self) -> Result<u64, TimeAbiError> {
        // The partition never sets the bus cycle, so it is KVM's default,
        // which KVM reports for the capability. Kernels before 6.11 lack the
        // capability (0) and have the same fixed 1 ns cycle.
        match self
            .kvm
            .check_extension(kvm::KVM_CAP_X86_APIC_BUS_CYCLES_NS)
        {
            Ok(0 | 1) => Ok(LAPIC_HZ_KVM),
            Ok(ns) => Err(TimeAbiError::new(
                TimeAbiCode::LapicRateUnavailable,
                format!("KVM's default APIC bus cycle is {ns} ns, not 1 ns"),
            )),
            Err(err) => Err(TimeAbiError::new(
                TimeAbiCode::LapicRateUnavailable,
                format!("cannot query KVM_CAP_X86_APIC_BUS_CYCLES_NS: {err:#}"),
            )),
        }
    }

    fn preflight(&self) -> Result<BackendPreflight, TimeAbiError> {
        // Partition creation fails with E_IDENTITY_ROUTING unless the MSR
        // filter and the user-space MSR exits are installed, so a built
        // partition always routes the identity MSRs to OpenVMM.
        let native_khz = self.native_tsc_hz()? / 1000;
        for (vp_index, vp) in self.time_abi_vps() {
            if !vp.has_tsc_offset_attr() {
                return Err(TimeAbiError::new(
                    TimeAbiCode::TscSyncUnsupported,
                    format!(
                        "VP {} lacks KVM_VCPU_TSC_OFFSET (Linux 5.16 or later)",
                        vp_index.index()
                    ),
                ));
            }
            let scaling = |message: String| {
                TimeAbiError::new(
                    TimeAbiCode::TscScalingActive,
                    format!("VP {}: {message}", vp_index.index()),
                )
            };
            let khz = vp.tsc_frequency_hz().map_err(|err| {
                TimeAbiError::new(
                    TimeAbiCode::TscRateUnavailable,
                    format!("KVM_GET_TSC_KHZ failed on VP {}: {err:#}", vp_index.index()),
                )
            })? / 1000;
            if khz != native_khz {
                return Err(scaling(format!(
                    "the TSC runs at {khz} kHz, VP 0 at {native_khz} kHz"
                )));
            }
            // Without scaling, the guest TSC is the host TSC plus the offset.
            // KVM scales only outside its tolerance (250 ppm by default),
            // which moves the value far outside the bracket once the host
            // has been up for about a second.
            let offset = vp
                .tsc_offset()
                .map_err(|err| scaling(format!("cannot read KVM_VCPU_TSC_OFFSET: {err:#}")))?;
            let mut guest_tsc = [0];
            let (bracket, result) =
                HostTscBracket::around(|| vp.get_msrs(&[x86defs::X86X_MSR_TSC], &mut guest_tsc));
            result.map_err(|err| scaling(format!("cannot read IA32_TSC: {err:#}")))?;
            if !tsc_is_unscaled(guest_tsc[0], offset, bracket) {
                return Err(scaling(format!(
                    "IA32_TSC {:#x} is not the host TSC in [{:#x}, {:#x}] plus the offset {offset:#x}",
                    guest_tsc[0], bracket.start, bracket.end
                )));
            }
        }
        tracing::info!(
            native_khz,
            vps = self.vps.len(),
            "time ABI KVM preflight: identity MSRs exit to OpenVMM, common TSC offset, no scaling"
        );
        Ok(BackendPreflight {
            msr_route: IdentityMsrRoute::ExitToVmm,
            sync: TscSyncMethod::CommonOffset,
        })
    }

    fn effective_cpuid(&self) -> Result<Vec<CpuidLeaf>, TimeAbiError> {
        Ok(cpuid_leaves(&self.bsp_cpuid))
    }

    fn supported_cpu_surface(&self) -> Result<Option<SupportedCpuSurface>, TimeAbiError> {
        Ok(self
            .time_abi
            .as_ref()
            .map(|time_abi| time_abi.surface.clone()))
    }

    fn capture_anchor(&self) -> Result<TscAnchor, TimeAbiError> {
        let tsc_hz = self.native_tsc_hz()?;
        // With every VP stopped, VP 0's TSC is the host TSC plus its offset
        // (preflight verified that it is not scaled). Reading the offset once
        // and pairing the host TSC with the host sample is much tighter than
        // pairing the sample with a KVM_GET_MSRS round trip.
        let offset = self.vp_kvm(VpIndex::BSP).tsc_offset().map_err(|err| {
            TimeAbiError::new(
                TimeAbiCode::TscAnchor,
                format!("cannot read VP 0's TSC offset: {err:#}"),
            )
        })?;
        let paired = paired_host_sample(tsc_hz, "capture anchor")?;
        let tsc = paired.host_tsc.wrapping_add(offset);
        tracing::debug!(
            tsc,
            sample = ?paired.sample,
            pairing_ns = paired.pairing_ns,
            "time ABI capture anchor"
        );
        Ok(TscAnchor {
            tsc,
            sample: paired.sample,
            pairing_ns: paired.pairing_ns,
        })
    }

    fn set_synchronized_tsc(
        &self,
        target: &mut dyn FnMut(&HostTimeSample) -> Result<u64, TimeAbiError>,
    ) -> Result<TscSetReport, TimeAbiError> {
        let readback_error =
            |message: String| TimeAbiError::new(TimeAbiCode::TscSyncReadback, message);
        let tsc_hz = self.native_tsc_hz()?;
        // The offsets before the set, logged for diagnosis: each vCPU's
        // creation-time offset, since a time ABI restore writes no saved TSC.
        let previous = self
            .time_abi_vps()
            .map(|(vp_index, vp)| {
                vp.tsc_offset().map_err(|err| {
                    readback_error(format!(
                        "cannot read VP {}'s TSC offset: {err:#}",
                        vp_index.index()
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        // The restore anchor. Every guest TSC equals the target at the host
        // instant of `host_tsc` and runs from there.
        let paired = paired_host_sample(tsc_hz, "restore anchor")?;
        let host_tsc = paired.host_tsc;
        let target_tsc = target(&paired.sample)?;
        let offset = target_tsc.wrapping_sub(host_tsc);
        for (vp_index, vp) in self.time_abi_vps() {
            vp.set_tsc_offset(offset).map_err(|err| {
                readback_error(format!(
                    "cannot set VP {}'s TSC offset to {offset:#x}: {err:#}",
                    vp_index.index()
                ))
            })?;
        }
        let readback = self
            .time_abi_vps()
            .map(|(vp_index, vp)| {
                vp.tsc_offset()
                    .map(|value| (vp_index, value))
                    .map_err(|err| {
                        readback_error(format!(
                            "cannot read back VP {}'s TSC offset: {err:#}",
                            vp_index.index()
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        check_offset_readback(offset, &readback)?;
        tracing::info!(
            target_tsc,
            host_tsc,
            offset = format_args!("{offset:#x}"),
            pairing_ns = paired.pairing_ns,
            vps = readback.len(),
            previous_minus_target_cycles = ?previous
                .iter()
                .map(|previous| host_tsc.wrapping_add(*previous).wrapping_sub(target_tsc) as i64)
                .collect::<Vec<_>>(),
            "time ABI synchronized TSC set"
        );
        Ok(TscSetReport {
            target: target_tsc,
            sample: paired.sample,
            readback,
            method: TscSyncMethod::CommonOffset,
        })
    }
}

/// Rejects partition capabilities that model Hyper-V (`hv1`), which the
/// identity leaves must not turn on. No KVM clock can appear: every KVM
/// signature leaf is removed.
pub(crate) fn check_capabilities(caps: &virt::PartitionCapabilities) -> Result<(), KvmError> {
    if caps.hv1 {
        return Err(KvmError::TimeAbi(routing_error(
            "partition capabilities expose hv1",
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use virt::time_abi::DeclaredRates;
    use virt::time_abi::identity::HYPERVISOR_CPUID_RANGE;
    use virt::time_abi::identity::time_abi_cpuid;
    use virt::time_abi::msr::MSR_APIC_FREQUENCY;
    use virt::time_abi::msr::MSR_TSC_FREQUENCY;
    use virt::time_abi::msr::MSR_VP_INDEX;

    #[test]
    fn filter_denies_exactly_the_owned_msrs() {
        let identity = [
            (0x4000_0000, 0x4000_01ff),
            (0x3b, 0x3b),
            (0x6e0, 0x6e0),
            (0x88, 0x8a),
            (0x116, 0x116),
            (0x118, 0x11b),
            (0x11e, 0x11e),
        ];
        let amd = [(0xc001_0015, 0xc001_0015), (0xc001_1029, 0xc001_1029)];
        assert_eq!(
            identity_msr_filter().to_vec(),
            time_abi_msr_filter(CpuVendor::Intel)
        );
        for (vendor, owned) in [
            (CpuVendor::Intel, identity.to_vec()),
            (CpuVendor::Amd, [identity.as_slice(), &amd].concat()),
        ] {
            let filter = time_abi_msr_filter(vendor);
            assert_eq!(filter.len(), owned.len(), "{vendor}");
            for (range, (first, last)) in filter.iter().zip(owned) {
                assert_eq!(range.base, first, "{vendor}");
                assert_eq!(range.base + range.nmsrs - 1, last, "{vendor}");
                assert!(range.read && range.write, "{vendor}");
                assert!(range.bitmap.iter().all(|&byte| byte == 0), "{vendor}");
            }
        }
    }

    #[test]
    fn legacy_l2_cache_msrs_are_the_legacy_stubs() {
        let hidden: Vec<u32> = LEGACY_L2_CACHE_MSRS.into_iter().flatten().collect();
        assert_eq!(hidden, super::super::MYSTERY_MSRS);
    }

    fn kvm_entry(function: u32, index: Option<u32>, result: [u32; 4]) -> kvm::kvm_cpuid_entry2 {
        kvm::kvm_cpuid_entry2 {
            function,
            index: index.unwrap_or(0),
            flags: if index.is_some() {
                kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX
            } else {
                0
            },
            eax: result[0],
            ebx: result[1],
            ecx: result[2],
            edx: result[3],
            ..Default::default()
        }
    }

    #[test]
    fn supported_surface_is_kvms_cpuid_width_and_arch_capabilities() {
        let supported = [
            kvm_entry(0, None, [0x16, 0x756e_6547, 0x6c65_746e, 0x4965_6e69]),
            kvm_entry(7, Some(0), [0, 0xd19f_4fbb, 0, 0xbc00_0400]),
            // A 46-bit physical address width.
            kvm_entry(0x8000_0008, None, [0x302e, 0, 0, 0]),
        ];
        let surface = supported_surface(&supported, 0x4000_0000_0c00_00eb);
        let leaves: Vec<_> = surface
            .cpuid
            .iter()
            .map(|leaf| (leaf.function, leaf.index, leaf.result))
            .collect();
        assert_eq!(
            leaves,
            [
                (0, None, [0x16, 0x756e_6547, 0x6c65_746e, 0x4965_6e69]),
                (7, Some(0), [0, 0xd19f_4fbb, 0, 0xbc00_0400]),
                (0x8000_0008, None, [0x302e, 0, 0, 0]),
            ]
        );
        assert_eq!(surface.physical_address_width, 46);
        let msrs: Vec<_> = surface
            .msrs
            .iter()
            .map(|msr| (msr.index, msr.supported, msr.controllable))
            .collect();
        assert_eq!(msrs, [(0x10a, 0x4000_0000_0c00_00eb, !0)]);
    }

    #[test]
    fn supported_surface_clears_psfd_without_spec_ctrl() {
        // AMD's PSFD (0x80000008 EBX[28]) without IBRS, STIBP, or SSBD: KVM
        // would not let the guest write SPEC_CTRL to use it.
        let psfd = 1 << 28;
        let lone = [kvm_entry(0x8000_0008, None, [0x30, psfd, 0, 0])];
        assert_eq!(supported_surface(&lone, 0).cpuid[0].result[1], 0);
        // With SSBD (bit 24), SPEC_CTRL is the guest's, and PSFD stays.
        let ssbd = 1 << 24;
        let with_ssbd = [kvm_entry(0x8000_0008, None, [0x30, psfd | ssbd, 0, 0])];
        assert_eq!(
            supported_surface(&with_ssbd, 0).cpuid[0].result[1],
            psfd | ssbd
        );
    }

    #[test]
    fn arch_capabilities_follow_the_profile() {
        // KVM 7.0 on a Skylake-SP host presents ITS_NO (bit 62), RFDS_NO
        // (27), SKIP_L1DFL_VMENTRY (3), and IF_PSCHANGE_MC_NO (6).
        let supported = (1 << 62) | (1 << 27) | (1 << 3) | (1 << 6);
        let enumerated = CpuidLeafSet::new(vec![CpuidLeaf::new(7, [0, 0, 0, 1 << 29]).indexed(0)]);
        // The Skylake profile pins its whole mask clear, ITS_NO included;
        // outside it, KVM's own value stays.
        let skylake = cpu_profile::pinned("intel.skylake-sp.v1").unwrap();
        assert_eq!(
            profile_arch_capabilities(skylake, supported, &enumerated),
            Some(1 << 6)
        );
        // The Ice Lake profile pins RDCL_NO, MDS_NO, TAA_NO, and RFDS_NO set.
        let icelake = cpu_profile::pinned("intel.icelake-sp.v1").unwrap();
        assert_eq!(
            profile_arch_capabilities(icelake, supported, &enumerated),
            Some(0x0800_0121 | (1 << 6))
        );
        // Without the CPUID enumeration the guest's reads raise #GP, and KVM
        // would refuse the write: nothing is presented.
        assert_eq!(
            profile_arch_capabilities(skylake, supported, &CpuidLeafSet::new(Vec::new())),
            None
        );
    }

    fn kvm_like_base() -> Vec<CpuidLeaf> {
        vec![
            CpuidLeaf::new(0, [0x16, 0x756e_6547, 0x6c65_746e, 0x4965_6e69]),
            CpuidLeaf::new(
                1,
                [
                    0x0005_0654,
                    0x0010_0800,
                    0xfffa_3203 | (1 << 24),
                    0x0f8b_fbff,
                ],
            ),
            CpuidLeaf::new(6, [0x4, 0, 0, 0]),
            CpuidLeaf::new(7, [0, 0xd19f_4fbb | (1 << 1), 0, 0]).indexed(0),
            CpuidLeaf::new(0xa, [0x0740_4f04, 0, 0, 0x603]),
            CpuidLeaf::new(0x15, [2, 0xd4, 0x017d_7840, 0]),
            // KVM's signature and features, and a stray leaf at another
            // hypervisor base.
            CpuidLeaf::new(0x4000_0000, [0x4000_0001, 0x4b4d_564b, 0x564b_4d56, 0x4d]),
            CpuidLeaf::new(0x4000_0001, [0x0100_8efb, 0, 0, 0]),
            CpuidLeaf::new(0x4000_0100, [0x4000_0101, 0x4b4d_564b, 0x564b_4d56, 0x4d]),
            CpuidLeaf::new(0x8000_0000, [0x8000_0008, 0, 0, 0]),
            CpuidLeaf::new(0x8000_0001, [0, 0, 0x121, 0x2c10_0800]),
            CpuidLeaf::new(0x8000_0007, [0, 0, 0, 0x100]),
        ]
    }

    /// An effective CPUID shaped like a CPU profile's: the CPU's leaves
    /// without a hypervisor leaf, and the time ABI's identity, zero, and time
    /// leaves.
    fn effective_like(vp_count: u32) -> CpuidLeafSet {
        let mut leaves: Vec<_> = kvm_like_base()
            .into_iter()
            .filter(|leaf| !HYPERVISOR_CPUID_RANGE.contains(&leaf.function))
            .collect();
        leaves.extend(time_abi_cpuid(vp_count, true).leaves().iter().copied());
        CpuidLeafSet::new(leaves)
    }

    #[test]
    fn partition_cpuid_is_the_time_abi_cpuid_verbatim() {
        let time_abi = effective_like(4);
        let cpuid = partition_cpuid(&time_abi);
        let leaves = |set: &CpuidLeafSet| -> Vec<_> {
            set.leaves()
                .iter()
                .map(|leaf| (leaf.function, leaf.index, leaf.result, leaf.mask))
                .collect()
        };
        assert_eq!(leaves(&cpuid), leaves(&time_abi));
        let mut lookup = |leaf, subleaf| cpuid.result(leaf, subleaf, &[0; 4]);
        virt::time_abi::identity::check_identity(&mut lookup, 4).unwrap();
        virt::time_abi::identity::check_time_bits(&mut lookup, true).unwrap();
        // No KVM signature at any base, and a leaf the table does not list
        // reads zero.
        assert_eq!(cpuid.result(0x4000_0100, 0, &[0; 4]), [0; 4]);
        assert_eq!(cpuid.result(0x8000_001f, 0, &[0; 4]), [0; 4]);
    }

    #[test]
    fn capabilities_of_the_partition_cpuid_exclude_hv1() {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .build(2)
            .unwrap();
        let cpuid = partition_cpuid(&effective_like(2));
        let mut lookup = |leaf, subleaf| {
            let mut result = cpuid.result(leaf, subleaf, &[0; 4]);
            if leaf == 1 {
                // Match the topology's APIC mode and drop XSAVE, which the
                // fake CPUID does not describe.
                result[2] &= !((1 << 21) | (1 << 26) | (1 << 27));
                if topology.apic_mode() != vm_topology::processor::x86::ApicMode::XApic {
                    result[2] |= 1 << 21;
                }
            }
            result
        };
        let caps = virt::PartitionCapabilities::from_cpuid(
            &topology,
            &mut virt::time_abi::identity::capabilities_cpuid(&mut lookup),
        )
        .unwrap();
        check_capabilities(&caps).unwrap();
        let unmasked = virt::PartitionCapabilities::from_cpuid(&topology, &mut lookup).unwrap();
        assert!(check_capabilities(&unmasked).is_err());
    }

    #[test]
    fn cpuid_leaves_keep_significant_indices() {
        let entries = [
            kvm::kvm_cpuid_entry2 {
                function: 7,
                index: 0,
                flags: kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
                ebx: 0x1234,
                ..Default::default()
            },
            kvm::kvm_cpuid_entry2 {
                function: 0x4000_0001,
                index: 0,
                flags: 0,
                eax: 0x3123_7648,
                ..Default::default()
            },
        ];
        let leaves = cpuid_leaves(&entries);
        assert_eq!(
            (leaves[0].function, leaves[0].index, leaves[0].result),
            (7, Some(0), [0, 0x1234, 0, 0])
        );
        assert_eq!(
            (leaves[1].function, leaves[1].index, leaves[1].result),
            (0x4000_0001, None, [0x3123_7648, 0, 0, 0])
        );
    }

    #[test]
    fn host_sample_pairing_uses_the_bracket_midpoint() {
        // 2 GHz: 2 cycles per ns.
        let hz = 2_000_000_000;
        let bracket = HostTscBracket {
            start: 1_000,
            end: 1_401,
        };
        // Midpoint 1200, half width 201 cycles rounds up to 101 ns.
        assert_eq!(pair_host_sample(bracket, hz), Some((1_200, 101)));
        // A half width of MAX_ANCHOR_PAIRING_NS (2 cycles per ns) is at the
        // anchor bound.
        let at_bound = HostTscBracket {
            start: 0,
            end: 4 * MAX_ANCHOR_PAIRING_NS,
        };
        assert_eq!(
            pair_host_sample(at_bound, hz),
            Some((2 * MAX_ANCHOR_PAIRING_NS, MAX_ANCHOR_PAIRING_NS))
        );
        let beyond = HostTscBracket {
            start: 0,
            end: 4 * MAX_ANCHOR_PAIRING_NS + 2,
        };
        assert_eq!(
            pair_host_sample(beyond, hz),
            Some((2 * MAX_ANCHOR_PAIRING_NS + 1, MAX_ANCHOR_PAIRING_NS + 1))
        );
        // A TSC that went backwards cannot be paired.
        let backwards = HostTscBracket { start: 10, end: 9 };
        assert_eq!(pair_host_sample(backwards, hz), None);
        // Near the top of the TSC range.
        let high = HostTscBracket {
            start: u64::MAX - 100,
            end: u64::MAX,
        };
        assert_eq!(pair_host_sample(high, hz), Some((u64::MAX - 50, 25)));
    }

    fn host_sample(ns: u64) -> HostTimeSample {
        HostTimeSample {
            utc_ns: ns,
            monotonic_ns: ns,
        }
    }

    #[test]
    fn anchors_sample_again_until_paired_within_the_bound() {
        // 1 GHz: one cycle per ns. Two brackets wider than the bound, then a
        // tight one.
        let wide = 2 * MAX_ANCHOR_PAIRING_NS;
        let mut brackets = [(0, wide + 10_000), (0, wide + 5_000), (1_000, 1_100)].into_iter();
        let paired = pair_within_bound(1_000_000_000, "capture anchor", || {
            let (start, end) = brackets.next().unwrap();
            (HostTscBracket { start, end }, Ok(host_sample(start)))
        })
        .unwrap();
        assert_eq!((paired.host_tsc, paired.pairing_ns), (1_050, 50));
        assert_eq!(paired.sample, host_sample(1_000));
    }

    #[test]
    fn anchors_fail_after_bounded_attempts() {
        let mut attempts = 0;
        let error = pair_within_bound(1_000_000_000, "restore anchor", || {
            attempts += 1;
            let end = 2 * MAX_ANCHOR_PAIRING_NS + attempts as u64;
            (HostTscBracket { start: 0, end }, Ok(host_sample(0)))
        })
        .unwrap_err();
        assert_eq!(attempts, ANCHOR_ATTEMPTS);
        assert_eq!(error.code, TimeAbiCode::TscAnchor);
        assert!(
            error.message.contains("restore anchor"),
            "{}",
            error.message
        );
        let tightest = format!("tightest pairing: {} ns", MAX_ANCHOR_PAIRING_NS + 1);
        assert!(error.message.contains(&tightest), "{}", error.message);

        let error = pair_within_bound(1_000_000_000, "capture anchor", || {
            (HostTscBracket { start: 10, end: 9 }, Ok(host_sample(0)))
        })
        .unwrap_err();
        assert_eq!(error.code, TimeAbiCode::TscAnchor);
        assert!(
            error.message.contains("went backwards"),
            "{}",
            error.message
        );

        // A host clock failure ends the anchor at once, with its own code.
        let error = pair_within_bound(1_000_000_000, "capture anchor", || {
            (
                HostTscBracket { start: 0, end: 1 },
                Err(TimeAbiError::new(TimeAbiCode::HostIdentity, "no clock")),
            )
        })
        .unwrap_err();
        assert_eq!(error.code, TimeAbiCode::HostIdentity);
    }

    #[test]
    fn unscaled_tsc_is_host_plus_offset() {
        let bracket = HostTscBracket {
            start: 5_000,
            end: 6_000,
        };
        // Offsets wrap: a guest TSC below the host TSC has an offset above
        // 2^63.
        for offset in [0, 1 << 40, 0u64.wrapping_sub(4_000)] {
            assert!(tsc_is_unscaled(
                5_500u64.wrapping_add(offset),
                offset,
                bracket
            ));
            assert!(tsc_is_unscaled(
                5_000u64.wrapping_add(offset),
                offset,
                bracket
            ));
            assert!(tsc_is_unscaled(
                6_000u64.wrapping_add(offset),
                offset,
                bracket
            ));
            assert!(!tsc_is_unscaled(
                4_999u64.wrapping_add(offset),
                offset,
                bracket
            ));
            assert!(!tsc_is_unscaled(
                6_001u64.wrapping_add(offset),
                offset,
                bracket
            ));
        }
        // 250 ppm of scaling at a host TSC of 10^12 is far outside the
        // bracket.
        let host = 1_000_000_000_000u64;
        let bracket = HostTscBracket {
            start: host,
            end: host + 20_000,
        };
        let scaled = host + host / 4_000;
        assert!(!tsc_is_unscaled(scaled, 0, bracket));
    }

    #[test]
    fn common_offset_reaches_the_target_at_the_anchor() {
        for (target, host_tsc) in [
            (5_000_000u64, 1_000u64),
            (1_000, 5_000_000),
            (u64::MAX, 0),
            (0, u64::MAX),
        ] {
            let offset = target.wrapping_sub(host_tsc);
            assert_eq!(host_tsc.wrapping_add(offset), target);
        }
    }

    #[test]
    fn offset_readback_requires_every_vp() {
        let vps = |values: &[u64]| -> Vec<(VpIndex, u64)> {
            values
                .iter()
                .enumerate()
                .map(|(index, &value)| (VpIndex::new(index as u32), value))
                .collect()
        };
        check_offset_readback(7, &vps(&[7, 7, 7, 7])).unwrap();
        check_offset_readback(7, &[]).unwrap();
        let error = check_offset_readback(7, &vps(&[7, 8, 7, 9])).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::TscSyncReadback);
        assert!(error.message.contains("VP 1: 0x8"), "{}", error.message);
        assert!(error.message.contains("VP 3: 0x9"), "{}", error.message);
    }

    fn time_abi(msrs: Arc<TimeAbiMsrs>, vendor: CpuVendor) -> KvmTimeAbi {
        KvmTimeAbi {
            msrs,
            kvm_invariant_control: None,
            surface: SupportedCpuSurface::default(),
            arch_capabilities: None,
            amd_config_msrs: vendor == CpuVendor::Amd,
        }
    }

    /// Maps an MSR exit result to `Some(Ok(value))`, `Some(Err(()))` for #GP,
    /// or `None` when the time ABI does not handle the MSR.
    fn outcome<T>(result: Option<Result<T, MsrError>>) -> Option<Result<T, ()>> {
        result.map(|result| {
            result.map_err(|err| assert!(matches!(err, MsrError::InvalidAccess), "{err:?}"))
        })
    }

    #[test]
    fn msr_exits_serve_the_identity_and_hide_tsc_adjust() {
        let msrs = Arc::new(TimeAbiMsrs::new());
        msrs.declare(DeclaredRates {
            tsc_hz: 2_194_804_000,
            apic_hz: LAPIC_HZ_KVM,
        })
        .unwrap();
        let time_abi = time_abi(msrs.clone(), CpuVendor::Intel);
        let vp = VpIndex::new(2);
        let read = |msr| outcome(time_abi.read_msr(vp, msr));
        let write = |msr, value| outcome(time_abi.write_msr(vp, msr, value));
        assert_eq!(read(MSR_VP_INDEX), Some(Ok(2)));
        assert_eq!(read(MSR_TSC_FREQUENCY), Some(Ok(2_194_804_000)));
        assert_eq!(read(MSR_APIC_FREQUENCY), Some(Ok(1_000_000_000)));
        // Hyper-V MSRs that KVM would emulate raise #GP.
        for msr in [
            0x4000_0000,
            0x4000_0001,
            0x4000_0020,
            0x4000_0021,
            0x4000_00b0,
        ] {
            assert_eq!(read(msr), Some(Err(())), "{msr:#x}");
            assert_eq!(write(msr, 1), Some(Err(())), "{msr:#x}");
        }
        assert_eq!(read(MSR_IA32_TSC_ADJUST), Some(Err(())));
        assert_eq!(write(MSR_IA32_TSC_ADJUST, 0), Some(Err(())));
        assert_eq!(read(MSR_IA32_TSC_DEADLINE), Some(Err(())));
        assert_eq!(write(MSR_IA32_TSC_DEADLINE, 1), Some(Err(())));
        // So do the legacy L2-cache MSRs that the legacy path stubs.
        for msr in [0x88, 0x89, 0x8a, 0x116, 0x118, 0x119, 0x11a, 0x11b, 0x11e] {
            assert_eq!(read(msr), Some(Err(())), "{msr:#x}");
            assert_eq!(write(msr, 0), Some(Err(())), "{msr:#x}");
        }
        // Other unknown MSRs keep their existing handling, and so do AMD's
        // configuration MSRs under an Intel profile, which the filter does
        // not route here.
        for msr in [0x87, 0x8b, 0x117, 0x11c, 0x11f, 0xc001_0015, 0xc001_1029] {
            assert_eq!(read(msr), None, "{msr:#x}");
        }
        assert_eq!(write(0x4000_0200, 0), None);
        // The invariant TSC control is guest state in the shared handler.
        assert_eq!(write(MSR_TSC_INVARIANT_CONTROL, 2), Some(Err(())));
        assert_eq!(write(MSR_TSC_INVARIANT_CONTROL, 1), Some(Ok(())));
        assert_eq!(msrs.tsc_invariant_control(), 1);
        assert_eq!(read(MSR_TSC_INVARIANT_CONTROL), Some(Ok(1)));
    }

    /// Under an AMD profile, the filter routes HWCR and DE_CFG to OpenVMM,
    /// which serves the time ABI's fixed values.
    #[test]
    fn msr_exits_serve_amd_config_msrs_under_an_amd_profile() {
        let time_abi = time_abi(Arc::new(TimeAbiMsrs::new()), CpuVendor::Amd);
        let vp = VpIndex::new(1);
        let read = |msr| outcome(time_abi.read_msr(vp, msr));
        let write = |msr, value| outcome(time_abi.write_msr(vp, msr, value));
        assert_eq!(read(0xc001_0015), Some(Ok(1 << 24)));
        assert_eq!(read(0xc001_1029), Some(Ok(1 << 1)));
        assert_eq!(write(0xc001_0015, 1 << 24), Some(Ok(())));
        assert_eq!(write(0xc001_0015, 1 << 24 | 1 << 30), Some(Err(())));
        assert_eq!(write(0xc001_1029, 0), Some(Err(())));
        assert_eq!(read(MSR_VP_INDEX), Some(Ok(1)));
        assert_eq!(read(MSR_IA32_TSC_ADJUST), Some(Err(())));
        assert_eq!(read(0xc001_0010), None);
    }

    #[test]
    fn cycle_conversion_rounds_up() {
        assert_eq!(cycles_to_ns_ceil(0, 2_000_000_000), 0);
        assert_eq!(cycles_to_ns_ceil(1, 2_000_000_000), 1);
        // One millisecond at the bare-metal KVM host's rate, and a hair over a
        // microsecond.
        assert_eq!(cycles_to_ns_ceil(2_194_804, 2_194_804_000), 1_000_000);
        assert_eq!(cycles_to_ns_ceil(2_195, 2_194_804_000), 1_001);
        assert_eq!(cycles_to_ns_ceil(u64::MAX, 1), u64::MAX);
    }
}

/// Tests against the host's KVM.
#[cfg(test)]
mod kvm_tests {
    use super::*;
    use test_with_tracing::test;

    fn partition(vp_count: u32) -> kvm::Partition {
        let kvm = kvm::Kvm::new().unwrap();
        let mut vm = kvm.new_vm(kvm::VmType::Default).unwrap();
        for vcpu in 0..vp_count {
            vm.add_vp(vcpu).unwrap();
        }
        vm
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn identity_msr_filter_installs() {
        let vm = partition(1);
        install_identity_msr_filter(&vm, CpuVendor::Amd).unwrap();
        install_identity_msr_filter(&vm, CpuVendor::Intel).unwrap();
        // Host-initiated accesses bypass the filter.
        let mut value = [0];
        vm.vp(0)
            .get_msrs(&[x86defs::X86X_MSR_TSC], &mut value)
            .unwrap();
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn common_offset_is_exact_on_every_vcpu() {
        for vp_count in [1, 2, 4, 8] {
            let vm = partition(vp_count);
            let hz = vm.vp(0).tsc_frequency_hz().unwrap();
            // Desynchronize the vCPUs first, as a restore's IA32_TSC writes
            // may.
            for vcpu in 0..vp_count {
                vm.vp(vcpu)
                    .set_tsc_offset(u64::from(vcpu) * 1_000_003)
                    .unwrap();
            }
            let (bracket, ()) = HostTscBracket::around(|| ());
            let target = hz * 3600;
            let offset = target.wrapping_sub(bracket.end);
            for vcpu in 0..vp_count {
                vm.vp(vcpu).set_tsc_offset(offset).unwrap();
            }
            for vcpu in 0..vp_count {
                let vp = vm.vp(vcpu);
                assert!(vp.has_tsc_offset_attr());
                assert_eq!(
                    vp.tsc_offset().unwrap(),
                    offset,
                    "vCPU {vcpu} of {vp_count}"
                );
                let mut guest = [0];
                let (bracket, result) =
                    HostTscBracket::around(|| vp.get_msrs(&[x86defs::X86X_MSR_TSC], &mut guest));
                result.unwrap();
                assert!(
                    tsc_is_unscaled(guest[0], offset, bracket),
                    "vCPU {vcpu} of {vp_count}: {:#x} {bracket:x?}",
                    guest[0]
                );
            }
        }
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn host_samples_pair_within_the_anchor_bound() {
        let vm = partition(1);
        let hz = vm.vp(0).tsc_frequency_hz().unwrap();
        let mut worst = 0;
        for _ in 0..1000 {
            worst = worst.max(paired_host_sample(hz, "test anchor").unwrap().pairing_ns);
        }
        tracing::info!(worst, "worst host sample pairing, ns");
        assert!(worst <= MAX_ANCHOR_PAIRING_NS);
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn invariant_control_mirror_reaches_kvm() {
        let vm = partition(2);
        let msrs = Arc::new(TimeAbiMsrs::new());
        let time_abi = KvmTimeAbi::new(
            &vm,
            &[0, 1],
            msrs.clone(),
            SupportedCpuSurface::default(),
            None,
            CpuVendor::Intel,
        )
        .unwrap();
        if time_abi.kvm_invariant_control.is_none() {
            tracing::info!("KVM does not implement HV_X64_MSR_TSC_INVARIANT_CONTROL");
            return;
        }
        msrs.write(VpIndex::BSP, MSR_TSC_INVARIANT_CONTROL, 1)
            .unwrap()
            .unwrap();
        time_abi.sync_invariant_control(&vm.vp(1)).unwrap();
        let mut value = [0];
        // The control is partition-wide: every vCPU reads the mirrored value.
        vm.vp(0)
            .get_msrs(&[MSR_TSC_INVARIANT_CONTROL], &mut value)
            .unwrap();
        assert_eq!(value[0], 1);
        // A reset returns it to 0, which a host write may set in KVM.
        msrs.reset();
        time_abi.sync_invariant_control(&vm.vp(0)).unwrap();
        vm.vp(1)
            .get_msrs(&[MSR_TSC_INVARIANT_CONTROL], &mut value)
            .unwrap();
        assert_eq!(value[0], 0);
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn profile_arch_capabilities_reach_the_vcpu() {
        let kvm = kvm::Kvm::new().unwrap();
        let surface = supported_surface(
            &kvm.supported_cpuid().unwrap(),
            kvm.feature_msr(MSR_IA32_ARCH_CAPABILITIES).unwrap(),
        );
        assert!(surface.physical_address_width >= 36);
        let supported = surface.msrs[0].supported;
        tracing::info!(
            supported = format_args!("{supported:#x}"),
            "KVM's ARCH_CAPABILITIES"
        );
        let vm = partition(1);
        // KVM accepts the MSR once the vCPU's CPUID enumerates it.
        let enumerated = CpuidLeafSet::new(vec![CpuidLeaf::new(7, [0, 0, 0, 1 << 29]).indexed(0)]);
        vm.vp(0)
            .set_cpuid(&[kvm::kvm_cpuid_entry2 {
                function: 7,
                flags: kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
                edx: 1 << 29,
                ..Default::default()
            }])
            .unwrap();
        let profile = cpu_profile::pinned("intel.skylake-sp.v1").unwrap();
        let value = profile_arch_capabilities(profile, supported, &enumerated).unwrap();
        let time_abi = KvmTimeAbi::new(
            &vm,
            &[0],
            Arc::new(TimeAbiMsrs::new()),
            surface,
            Some(value),
            profile.cpu_vendor(),
        )
        .unwrap();
        time_abi.set_arch_capabilities(&vm.vp(0)).unwrap();
        let mut read = [0];
        vm.vp(0)
            .get_msrs(&[MSR_IA32_ARCH_CAPABILITIES], &mut read)
            .unwrap();
        assert_eq!(read[0], value);
        assert_eq!(read[0] & (1 << 62), 0, "ITS_NO is pinned clear");
    }
}
