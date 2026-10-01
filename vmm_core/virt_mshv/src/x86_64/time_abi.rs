// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVX time ABI on MSHV.
//!
//! A partition built with a [`TimeAbiConfig`] shows the guest the time ABI's
//! minimal Hyper-V identity, served by OpenVMM rather than by the hypervisor:
//!
//! - The partition has no synthetic processor features, so the hypervisor
//!   reports no hypervisor CPUID leaves and serves no synthetic MSR itself.
//!   With them, it would serve `HV_X64_MSR_VP_INDEX` and the frequency MSRs
//!   natively and never deliver their intercepts, and
//!   `HV_X64_MSR_TSC_FREQUENCY` would report this host's rate instead of the
//!   declared one.
//! - MSR-index intercepts route reads of the four identity MSRs and writes of
//!   `HV_X64_MSR_TSC_INVARIANT_CONTROL` to [`TimeAbiMsrs`]. The hypervisor
//!   raises #GP itself for writes to the three read-only identity MSRs, even
//!   when they are intercepted, and for every other MSR of the identity range.
//! - CPUID intercept results apply the configured CPUID over the hypervisor's
//!   own. The hypervisor reports no hypervisor leaf of its own, which
//!   preflight verifies at sentinel leaves.
//! - The processor feature banks keep the invariant TSC and hide the
//!   TSC-deadline timer, `IA32_TSC_ADJUST`, and APERF/MPERF.
//! - The synchronized TSC set freezes partition time, writes the target to
//!   every created VP, and reads it back. Partition time thaws when a VP first
//!   runs, so the guest resumes exactly at the target.
//!
//! These semantics were verified on hypervisor builds 10.0.26100.30000 (bare
//! metal) and 10.0.26100.9444 (Azure nested), where they are identical.

use crate::KernelError;
use crate::MshvPartition;
use crate::MshvPartitionInner;
use crate::MshvProcessor;
use crate::VcpuFdExt;
use hvdef::HvInterceptAccessType;
use hvdef::HvMessage;
use hvdef::HvPartitionPropertyCode;
use hvdef::HvX64RegisterName;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_READ_WRITE;
use hvdef::hypercall::HvRegisterAssoc;
use inspect::Inspect;
use mshv_ioctls::VcpuFd;
use mshv_ioctls::VmFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use virt::CpuidLeaf;
use virt::CpuidLeafSet;
use virt::VpIndex;
use virt::time_abi::BackendPreflight;
use virt::time_abi::HostTimeSample;
use virt::time_abi::IdentityMsrRoute;
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
use virt::time_abi::msr::MSR_APIC_FREQUENCY;
use virt::time_abi::msr::MSR_TSC_FREQUENCY;
use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
use virt::time_abi::msr::MSR_VP_INDEX;

/// The identity MSRs that OpenVMM serves. The hypervisor raises #GP for
/// every other MSR of the identity range.
pub(crate) const ROUTED_MSRS: [u32; 4] = [
    MSR_VP_INDEX,
    MSR_TSC_FREQUENCY,
    MSR_APIC_FREQUENCY,
    MSR_TSC_INVARIANT_CONTROL,
];

/// The most a capture anchor's host sample may differ from the instant VP 0's
/// TSC was read, in nanoseconds.
pub(crate) const MAX_ANCHOR_PAIRING_NS: u64 = 10_000;

/// How many bracketed TSC reads a capture anchor tries before giving up.
const ANCHOR_ATTEMPTS: usize = 64;

/// A capture anchor pairing this tight ends the search early.
const ANCHOR_TARGET_PAIRING_NS: u64 = 1_000;

/// Leaves read from VP 0 for the effective CPUID besides the programmed ones:
/// the maximum basic and extended leaves.
const EFFECTIVE_CPUID_MAX_LEAVES: [u32; 2] = [0, 0x8000_0000];

/// CPUID leaves of the identity range that preflight reads from VP 0 to check
/// that the zero leaves took effect: the first one past the identity, the
/// Hyper-V isolation leaf `0x40000081` (where `"VS#1"` would appear), and the
/// last one, plus three hypervisor signature bases.
const PREFLIGHT_SENTINEL_LEAVES: [u32; 6] = [
    0x4000_0006,
    0x4000_0081,
    0x4000_00ff,
    0x4000_0100,
    0x4000_0200,
    0x4000_ff00,
];

/// Time ABI state of an MSHV partition.
#[derive(Inspect)]
pub(crate) struct MshvTimeAbi {
    /// The identity MSR handler, shared with the worker.
    #[inspect(
        rename = "tsc_invariant_control",
        with = "|x| x.tsc_invariant_control()"
    )]
    msrs: Arc<TimeAbiMsrs>,
    /// The CPUID results registered with the hypervisor.
    #[inspect(skip)]
    registered_cpuid: Vec<CpuidLeaf>,
    /// Set by the synchronized TSC set. VP creation fails afterwards.
    #[inspect(with = "|x| x.load(Ordering::Relaxed)")]
    vp_set_sealed: AtomicBool,
}

impl MshvTimeAbi {
    pub(super) fn new(config: &TimeAbiConfig, registered_cpuid: &CpuidLeafSet) -> Self {
        Self {
            msrs: config.msrs.clone(),
            registered_cpuid: registered_cpuid.leaves().to_vec(),
            vp_set_sealed: AtomicBool::new(false),
        }
    }

    /// Fails with `E_VP_LATE_CREATION` once the synchronized TSC set has run.
    ///
    /// VP creation checks this both before and after it marks a VP created,
    /// and the synchronized set seals before it collects the created VPs, so a
    /// VP is either written by the set or fails creation.
    pub(super) fn check_vp_creation(&self, vp_index: VpIndex) -> Result<(), TimeAbiError> {
        if self.vp_set_sealed.load(Ordering::SeqCst) {
            return Err(TimeAbiError::new(
                TimeAbiCode::VpLateCreation,
                format!(
                    "VP {} was instantiated after the synchronized TSC set",
                    vp_index.index()
                ),
            ));
        }
        Ok(())
    }
}

/// Fails unless the partition can carry the time ABI: it must not be isolated
/// and must not expose the Hyper-V guest interface, whose synthetic processor
/// features would serve the identity MSRs natively.
pub(super) fn validate_partition(
    isolation: virt::IsolationType,
    hv_configured: bool,
) -> Result<(), TimeAbiError> {
    if isolation != virt::IsolationType::None {
        return Err(TimeAbiError::new(
            TimeAbiCode::IdentityRouting,
            format!("the MSHV time ABI does not support {isolation:?} isolation"),
        ));
    }
    if hv_configured {
        return Err(TimeAbiError::new(
            TimeAbiCode::IdentityRouting,
            "the MSHV time ABI cannot be combined with the Hyper-V guest interface",
        ));
    }
    Ok(())
}

/// Returns processor feature bank 1 for a time ABI partition: `supported`,
/// with the invariant TSC, and without the TSC-deadline timer,
/// `IA32_TSC_ADJUST`, or APERF/MPERF.
pub(super) fn processor_features1(
    supported: hvdef::HvX64PartitionProcessorFeatures1,
) -> hvdef::HvX64PartitionProcessorFeatures1 {
    supported
        .with_tsc_invariant_support(true)
        .with_tsc_deadline_tmr_support(false)
        .with_tsc_adjust_support(false)
        .with_a_count_m_count_support(false)
}

/// Applies the time ABI's processor feature bank 1 to the partition creation
/// arguments, whose banks hold the disabled features.
pub(super) fn with_feature_banks(
    mut args: mshv_bindings::mshv_create_partition_v2,
) -> mshv_bindings::mshv_create_partition_v2 {
    let mut banks = args.pt_cpu_fbanks;
    banks[1] = !u64::from(processor_features1(super::supported_processor_features1()));
    args.pt_cpu_fbanks = banks;
    args
}

/// Returns the MSR-index intercept that routes reads and writes of `msr` to
/// OpenVMM.
fn msr_index_intercept(msr: u32) -> mshv_bindings::mshv_install_intercept {
    mshv_bindings::mshv_install_intercept {
        access_type_mask: HV_INTERCEPT_ACCESS_MASK_READ_WRITE,
        intercept_type: mshv_bindings::hv_intercept_type_HV_INTERCEPT_TYPE_X64_MSR_INDEX,
        intercept_parameter: mshv_bindings::hv_intercept_parameters {
            as_uint64: u64::from(msr),
        },
    }
}

/// Routes the identity MSRs to OpenVMM. Fails with `E_IDENTITY_ROUTING`,
/// naming the MSR whose intercept the hypervisor refused.
pub(super) fn route_identity_msrs(vmfd: &VmFd) -> Result<(), TimeAbiError> {
    install_msr_intercepts(vmfd, &ROUTED_MSRS)
}

fn install_msr_intercepts(vmfd: &VmFd, msrs: &[u32]) -> Result<(), TimeAbiError> {
    for &msr in msrs {
        vmfd.install_intercept(msr_index_intercept(msr))
            .map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::IdentityRouting,
                    format!(
                        "cannot intercept MSR {msr:#x}: {}",
                        error_chain(&KernelError::from(error))
                    ),
                )
            })?;
    }
    Ok(())
}

/// Returns the CPUID results to register for a time ABI partition.
///
/// The hypervisor-range leaves of `own`, the backend's other CPUID results,
/// are dropped, and `config` is applied last, over everything else. Without
/// synthetic processor features, the hypervisor itself reports every
/// hypervisor-range leaf as zero, so the identity range holds exactly the
/// configured leaves (the identity and the explicit zero leaves) and zeros.
pub(super) fn partition_cpuid(own: Vec<CpuidLeaf>, config: &CpuidLeafSet) -> CpuidLeafSet {
    let mut leaves: Vec<CpuidLeaf> = own
        .into_iter()
        .filter(|leaf| !HYPERVISOR_CPUID_RANGE.contains(&leaf.function))
        .collect();
    leaves.extend(config.leaves().iter().copied());
    CpuidLeafSet::new(leaves)
}

/// Returns whether `leaf` is an explicit zero leaf past the identity, which
/// the effective CPUID reports as registered instead of reading it back.
/// Preflight reads such leaves back at its sentinels.
fn is_zero_fill(leaf: &CpuidLeaf) -> bool {
    leaf.function > IDENTITY_MAX_LEAF
        && IDENTITY_CPUID_RANGE.contains(&leaf.function)
        && leaf.result == [0; 4]
        && leaf.mask == [!0; 4]
}

/// An intercepted access to an identity MSR.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum MsrAccess {
    Read,
    Write(u64),
}

/// How an intercepted identity MSR access completes.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum MsrOutcome {
    /// The read returns this value.
    Read(u64),
    /// The write takes effect.
    Written,
    /// The access raises #GP.
    Fault,
}

/// Serves an intercepted access through the identity MSR handler.
fn serve_identity_msr(msrs: &TimeAbiMsrs, vp: VpIndex, msr: u32, access: MsrAccess) -> MsrOutcome {
    let outcome = match access {
        MsrAccess::Read => msrs.read(vp, msr).map(|r| r.map(MsrOutcome::Read)),
        MsrAccess::Write(value) => msrs
            .write(vp, msr, value)
            .map(|r| r.map(|()| MsrOutcome::Written)),
    };
    match outcome {
        Some(Ok(outcome)) => outcome,
        Some(Err(_)) => MsrOutcome::Fault,
        None => {
            // Only identity MSRs are intercepted, so this is a hypervisor
            // defect; the access still faults as an unknown MSR would.
            tracelimit::error_ratelimited!(
                vp = vp.index(),
                msr,
                "MSR intercept outside the time ABI identity range"
            );
            MsrOutcome::Fault
        }
    }
}

/// Returns the pending-event register value that raises #GP(0).
fn gp_fault_event() -> u128 {
    hvdef::HvX64PendingExceptionEvent::new()
        .with_event_pending(true)
        .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
        .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into())
        .with_deliver_error_code(true)
        .with_error_code(0)
        .into()
}

impl MshvProcessor<'_> {
    /// Completes an intercepted identity MSR access, or raises #GP.
    pub(super) fn handle_time_abi_msr_intercept(
        &mut self,
        time_abi: &MshvTimeAbi,
        message: &HvMessage,
    ) {
        let info = message.as_message::<hvdef::HvX64MsrInterceptMessage>();
        let access = if info.header.intercept_access_type == HvInterceptAccessType::WRITE {
            MsrAccess::Write((info.rdx << 32) | (info.rax & 0xffff_ffff))
        } else {
            MsrAccess::Read
        };
        let outcome = serve_identity_msr(&time_abi.msrs, self.vpindex, info.msr_number, access);
        tracing::trace!(
            vp = self.vpindex.index(),
            msr = info.msr_number,
            ?access,
            ?outcome,
            "time ABI identity MSR access"
        );
        let next_rip = info.header.rip + u64::from(info.header.instruction_len());
        match outcome {
            MsrOutcome::Read(value) => {
                let rp = self.runner.reg_page();
                rp.gp_registers[x86emu::Gp::RAX as usize] = value & 0xffff_ffff;
                rp.gp_registers[x86emu::Gp::RDX as usize] = value >> 32;
                rp.rip = next_rip;
                rp.dirty.set_general_purpose(true);
                rp.dirty.set_instruction_pointer(true);
            }
            MsrOutcome::Written => {
                let rp = self.runner.reg_page();
                rp.rip = next_rip;
                rp.dirty.set_instruction_pointer(true);
            }
            MsrOutcome::Fault => {
                // The faulting instruction does not retire, so RIP stays.
                self.runner
                    .vcpufd
                    .set_hvdef_regs(&[HvRegisterAssoc::from((
                        HvX64RegisterName::PendingEvent0,
                        gp_fault_event(),
                    ))])
                    .expect("failed to inject #GP for an identity MSR access");
            }
        }
    }
}

/// Returns the VPs that exist in the hypervisor, given whether each VP of the
/// topology was created. A restore-time VP prefix leaves the suffix uncreated.
fn created_vps(created: impl IntoIterator<Item = bool>) -> Vec<VpIndex> {
    created
        .into_iter()
        .enumerate()
        .filter_map(|(index, created)| created.then_some(VpIndex::new(index as u32)))
        .collect()
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
    /// read instant by at most half the bracket. UTC is derived from the first
    /// sample and the monotonic midpoint, so a UTC step inside the bracket
    /// does not skew it.
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
/// within [`MAX_ANCHOR_PAIRING_NS`].
fn select_anchor(candidates: &[AnchorCandidate]) -> Result<TscAnchor, TimeAbiError> {
    let best = candidates
        .iter()
        .min_by_key(|candidate| candidate.width_ns())
        .ok_or_else(|| TimeAbiError::new(TimeAbiCode::TscAnchor, "no TSC read was attempted"))?;
    let anchor = best.anchor();
    if anchor.pairing_ns > MAX_ANCHOR_PAIRING_NS {
        return Err(TimeAbiError::new(
            TimeAbiCode::TscAnchor,
            format!(
                "the tightest of {} VP 0 TSC reads pairs within {} ns, above the {MAX_ANCHOR_PAIRING_NS} ns bound",
                candidates.len(),
                anchor.pairing_ns
            ),
        ));
    }
    Ok(anchor)
}

/// Formats an error with its sources, for failure messages.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        text.push_str(": ");
        text.push_str(&error.to_string());
        source = error.source();
    }
    text
}

/// The input of `HvCallGetVpRegisters` and `HvCallSetVpRegisters` for one
/// register of another VP.
#[repr(C)]
struct VpRegisterInput {
    header: hvdef::hypercall::GetSetVpRegisters,
    register: HvRegisterAssoc,
}

fn vp_register_input(vp: VpIndex, register: HvRegisterAssoc) -> VpRegisterInput {
    VpRegisterInput {
        header: hvdef::hypercall::GetSetVpRegisters {
            partition_id: 0,
            vp_index: vp.index(),
            target_vtl: hvdef::hypercall::HvInputVtl::CURRENT_VTL,
            rsvd: [0; 3],
        },
        register,
    }
}

/// Writes the TSC of a created VP through `HvCallSetVpRegisters`.
fn set_vp_tsc(vmfd: &VmFd, vp: VpIndex, tsc: u64) -> Result<(), KernelError> {
    let input = vp_register_input(vp, HvRegisterAssoc::from((HvX64RegisterName::Tsc, tsc)));
    let mut args = mshv_bindings::mshv_root_hvcall {
        code: hvdef::HypercallCode::HvCallSetVpRegisters.0,
        in_sz: size_of::<VpRegisterInput>() as u16,
        in_ptr: std::ptr::addr_of!(input) as u64,
        reps: 1,
        ..Default::default()
    };
    vmfd.hvcall(&mut args)?;
    if args.reps != 1 {
        return Err(KernelError::Kernel(std::io::Error::from_raw_os_error(
            libc::EINTR,
        )));
    }
    Ok(())
}

/// Reads the TSC of a created VP through `HvCallGetVpRegisters`.
fn get_vp_tsc(vmfd: &VmFd, vp: VpIndex) -> Result<u64, KernelError> {
    // The input is the header and one register name; the name occupies the
    // first four bytes of the register association.
    let input = vp_register_input(vp, HvRegisterAssoc::from((HvX64RegisterName::Tsc, 0u64)));
    let mut output = [0u64; 2];
    let mut args = mshv_bindings::mshv_root_hvcall {
        code: hvdef::HypercallCode::HvCallGetVpRegisters.0,
        in_sz: (size_of::<hvdef::hypercall::GetSetVpRegisters>() + size_of::<u32>()) as u16,
        in_ptr: std::ptr::addr_of!(input) as u64,
        out_sz: size_of_val(&output) as u16,
        out_ptr: output.as_mut_ptr() as u64,
        reps: 1,
        ..Default::default()
    };
    vmfd.hvcall(&mut args)?;
    if args.reps != 1 {
        return Err(KernelError::Kernel(std::io::Error::from_raw_os_error(
            libc::EINTR,
        )));
    }
    Ok(output[0])
}

/// Reads one CPUID leaf as VP 0 sees it.
fn vp_cpuid(bsp: &VcpuFd, function: u32, index: u32) -> Result<[u32; 4], KernelError> {
    Ok(bsp.get_cpuid_values(function, index, 0, 0)?)
}

impl MshvPartitionInner {
    fn time_abi_state(&self) -> Result<&MshvTimeAbi, TimeAbiError> {
        self.time_abi.as_ref().ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::TscSyncUnsupported,
                "the partition was built without the time ABI",
            )
        })
    }

    fn time_abi_bsp(&self, code: TimeAbiCode) -> Result<&VcpuFd, TimeAbiError> {
        self.finalized()
            .map(|finalized| &finalized.bsp_vcpufd)
            .map_err(|error| TimeAbiError::new(code, format!("VP 0 is unavailable: {error}")))
    }

    /// Checks that partition time can be frozen, leaving it as it was: a
    /// frozen partition stays frozen until a VP runs.
    fn probe_time_freeze(&self) -> Result<(), KernelError> {
        let frozen = self.time_frozen.lock();
        self.vmfd
            .set_partition_property(HvPartitionPropertyCode::TimeFreeze.0, 1)?;
        if !*frozen {
            self.vmfd
                .set_partition_property(HvPartitionPropertyCode::TimeFreeze.0, 0)?;
        }
        Ok(())
    }

    /// Checks that VP 0 sees the identity leaves as registered, and zero at
    /// the sentinel leaves.
    fn check_identity_cpuid(&self, bsp: &VcpuFd) -> Result<(), TimeAbiError> {
        let state = self.time_abi_state()?;
        let identity = state
            .registered_cpuid
            .iter()
            .filter(|leaf| {
                leaf.function <= IDENTITY_MAX_LEAF && IDENTITY_CPUID_RANGE.contains(&leaf.function)
            })
            .copied();
        let sentinels = PREFLIGHT_SENTINEL_LEAVES
            .into_iter()
            .map(|function| CpuidLeaf::new(function, [0; 4]));
        for leaf in identity.chain(sentinels) {
            let function = leaf.function;
            let actual = vp_cpuid(bsp, function, 0).map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::IdentityRouting,
                    format!(
                        "cannot read CPUID {function:#x} of VP 0: {}",
                        error_chain(&error)
                    ),
                )
            })?;
            let mut expected = actual;
            leaf.apply(&mut expected);
            if actual != expected {
                return Err(TimeAbiError::new(
                    TimeAbiCode::IdentityRouting,
                    format!(
                        "VP 0 reads CPUID {function:#x} as {actual:#x?}, expected {expected:#x?}"
                    ),
                ));
            }
        }
        Ok(())
    }
}

impl TimeAbiBackend for MshvPartition {
    fn native_tsc_hz(&self) -> Result<u64, TimeAbiError> {
        let hz = self
            .inner
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::ProcessorClockFrequency.0)
            .map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::TscRateUnavailable,
                    format!(
                        "cannot read the ProcessorClockFrequency partition property: {}",
                        error_chain(&KernelError::from(error))
                    ),
                )
            })?;
        if hz == 0 {
            return Err(TimeAbiError::new(
                TimeAbiCode::TscRateUnavailable,
                "the ProcessorClockFrequency partition property is zero",
            ));
        }
        Ok(hz)
    }

    fn lapic_hz(&self) -> Result<u64, TimeAbiError> {
        let hz = self
            .inner
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::ApicFrequency.0)
            .map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::LapicRateUnavailable,
                    format!(
                        "cannot read the ApicFrequency partition property: {}",
                        error_chain(&KernelError::from(error))
                    ),
                )
            })?;
        if hz == 0 {
            return Err(TimeAbiError::new(
                TimeAbiCode::LapicRateUnavailable,
                "the ApicFrequency partition property is zero",
            ));
        }
        Ok(hz)
    }

    fn preflight(&self) -> Result<BackendPreflight, TimeAbiError> {
        // The MSR intercepts were installed at partition creation, which
        // fails with E_IDENTITY_ROUTING otherwise. Check that the CPUID
        // results took effect.
        let bsp = self.inner.time_abi_bsp(TimeAbiCode::IdentityRouting)?;
        self.inner.check_identity_cpuid(bsp)?;

        self.inner.probe_time_freeze().map_err(|error| {
            TimeAbiError::new(
                TimeAbiCode::TscSyncUnsupported,
                format!("cannot freeze partition time: {}", error_chain(&error)),
            )
        })?;

        // The guest TSC is the host TSC plus an offset: OpenVMM never sets a
        // TSC frequency or multiplier for an MSHV partition.
        Ok(BackendPreflight {
            msr_route: IdentityMsrRoute::ExitToVmm,
            sync: TscSyncMethod::FrozenWrite,
        })
    }

    fn effective_cpuid(&self) -> Result<Vec<CpuidLeaf>, TimeAbiError> {
        let state = self.inner.time_abi_state()?;
        let bsp = self.inner.time_abi_bsp(TimeAbiCode::CpuSurface)?;
        let read = |function: u32, index: Option<u32>| {
            let result = vp_cpuid(bsp, function, index.unwrap_or(0)).map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::CpuSurface,
                    format!(
                        "cannot read CPUID {function:#x}/{:#x} of VP 0: {}",
                        index.unwrap_or(0),
                        error_chain(&error)
                    ),
                )
            })?;
            let leaf = CpuidLeaf::new(function, result);
            Ok(match index {
                Some(index) => leaf.indexed(index),
                None => leaf,
            })
        };
        let mut leaves = EFFECTIVE_CPUID_MAX_LEAVES
            .into_iter()
            .map(|function| read(function, None))
            .collect::<Result<Vec<_>, TimeAbiError>>()?;
        for leaf in &state.registered_cpuid {
            leaves.push(if is_zero_fill(leaf) {
                *leaf
            } else {
                read(leaf.function, leaf.index)?
            });
        }
        Ok(CpuidLeafSet::new(leaves).into_leaves())
    }

    fn capture_anchor(&self) -> Result<TscAnchor, TimeAbiError> {
        let mut candidates = Vec::with_capacity(ANCHOR_ATTEMPTS);
        for _ in 0..ANCHOR_ATTEMPTS {
            let before = sample_host_time()?;
            let tsc = get_vp_tsc(&self.inner.vmfd, VpIndex::BSP).map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::TscAnchor,
                    format!("cannot read the TSC of VP 0: {}", error_chain(&error)),
                )
            })?;
            let after = sample_host_time()?;
            let candidate = AnchorCandidate { tsc, before, after };
            candidates.push(candidate);
            if candidate.width_ns() <= 2 * ANCHOR_TARGET_PAIRING_NS {
                break;
            }
        }
        let anchor = select_anchor(&candidates)?;
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
        let state = self.inner.time_abi_state()?;
        // Seal before collecting the created VPs; see
        // `MshvTimeAbi::check_vp_creation`.
        state.vp_set_sealed.store(true, Ordering::SeqCst);
        let created = created_vps(
            self.inner
                .vps
                .iter()
                .map(|vp| vp.created.load(Ordering::SeqCst)),
        );
        if created.first() != Some(&VpIndex::BSP) {
            return Err(TimeAbiError::new(
                TimeAbiCode::TscSyncUnsupported,
                "VP 0 does not exist",
            ));
        }

        // Partition time stays frozen until a VP first runs, so every VP
        // holds the target from the anchor until the guest resumes.
        self.inner.freeze_time().map_err(|error| {
            TimeAbiError::new(
                TimeAbiCode::TscSyncUnsupported,
                format!("cannot freeze partition time: {}", error_chain(&error)),
            )
        })?;
        let frozen = started.elapsed();

        let sample = sample_host_time()?;
        let target = target(&sample)?;

        let vmfd = &self.inner.vmfd;
        let failed = |vp: VpIndex, what: &str, error: KernelError| {
            TimeAbiError::new(
                TimeAbiCode::TscSyncReadback,
                format!(
                    "cannot {what} the TSC of VP {}: {}",
                    vp.index(),
                    error_chain(&error)
                ),
            )
        };
        for &vp in &created {
            set_vp_tsc(vmfd, vp, target).map_err(|error| failed(vp, "write", error))?;
        }
        let written = started.elapsed();
        let readback = created
            .iter()
            .map(|&vp| {
                get_vp_tsc(vmfd, vp)
                    .map(|value| (vp, value))
                    .map_err(|error| failed(vp, "read back", error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = check_readback(target, &readback);
        tracing::info!(
            created_vps = created.len(),
            vp_capacity = self.inner.vps.len(),
            target,
            readback_equal = result.is_ok(),
            freeze_us = frozen.as_micros() as u64,
            write_us = (written - frozen).as_micros() as u64,
            total_us = started.elapsed().as_micros() as u64,
            "time ABI: synchronized TSC set while partition time is frozen"
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

#[cfg(test)]
mod tests {
    use super::*;
    use virt::time_abi::DeclaredRates;
    use virt::time_abi::identity::time_abi_cpuid;

    const VP_COUNT: u32 = 8;

    fn declared_msrs() -> TimeAbiMsrs {
        let msrs = TimeAbiMsrs::new();
        msrs.declare(DeclaredRates::new(2_194_844_582, 200_000_000).unwrap())
            .unwrap();
        msrs
    }

    #[test]
    fn identity_msr_reads_follow_the_abi_table() {
        let msrs = declared_msrs();
        let read = |vp, msr| serve_identity_msr(&msrs, VpIndex::new(vp), msr, MsrAccess::Read);
        assert_eq!(read(3, MSR_VP_INDEX), MsrOutcome::Read(3));
        assert_eq!(read(0, MSR_TSC_FREQUENCY), MsrOutcome::Read(2_194_844_582));
        assert_eq!(read(5, MSR_APIC_FREQUENCY), MsrOutcome::Read(200_000_000));
        assert_eq!(read(1, MSR_TSC_INVARIANT_CONTROL), MsrOutcome::Read(0));
    }

    #[test]
    fn frequency_reads_fault_before_the_rates_are_declared() {
        let msrs = TimeAbiMsrs::new();
        for msr in [MSR_TSC_FREQUENCY, MSR_APIC_FREQUENCY] {
            assert_eq!(
                serve_identity_msr(&msrs, VpIndex::BSP, msr, MsrAccess::Read),
                MsrOutcome::Fault
            );
        }
        assert_eq!(
            serve_identity_msr(&msrs, VpIndex::new(2), MSR_VP_INDEX, MsrAccess::Read),
            MsrOutcome::Read(2)
        );
    }

    #[test]
    fn tsc_invariant_control_accepts_only_zero_and_one() {
        let msrs = declared_msrs();
        let vp = VpIndex::new(1);
        let read = || serve_identity_msr(&msrs, vp, MSR_TSC_INVARIANT_CONTROL, MsrAccess::Read);
        let write = |value| {
            serve_identity_msr(
                &msrs,
                vp,
                MSR_TSC_INVARIANT_CONTROL,
                MsrAccess::Write(value),
            )
        };
        for value in [1, 0, 1] {
            assert_eq!(write(value), MsrOutcome::Written);
            assert_eq!(read(), MsrOutcome::Read(value));
        }
        for value in [2, 3, (1 << 63) | 1, u64::MAX] {
            assert_eq!(write(value), MsrOutcome::Fault);
            assert_eq!(read(), MsrOutcome::Read(1));
        }
    }

    #[test]
    fn read_only_and_undefined_identity_msrs_fault() {
        let msrs = declared_msrs();
        for msr in [MSR_VP_INDEX, MSR_TSC_FREQUENCY, MSR_APIC_FREQUENCY] {
            assert_eq!(
                serve_identity_msr(&msrs, VpIndex::BSP, msr, MsrAccess::Write(0)),
                MsrOutcome::Fault
            );
        }
        for msr in [
            0x4000_0000,
            0x4000_0001,
            0x4000_0020,
            0x4000_0021,
            0x4000_01ff,
        ] {
            assert_eq!(
                serve_identity_msr(&msrs, VpIndex::BSP, msr, MsrAccess::Read),
                MsrOutcome::Fault
            );
            assert_eq!(
                serve_identity_msr(&msrs, VpIndex::BSP, msr, MsrAccess::Write(1)),
                MsrOutcome::Fault
            );
        }
    }

    #[test]
    fn msrs_outside_the_identity_range_fault() {
        let msrs = declared_msrs();
        for msr in [0x10, 0x3fff_ffff, 0x4000_0200] {
            assert_eq!(
                serve_identity_msr(&msrs, VpIndex::BSP, msr, MsrAccess::Read),
                MsrOutcome::Fault
            );
        }
    }

    #[test]
    fn routed_msrs_are_the_served_identity_msrs() {
        assert_eq!(
            ROUTED_MSRS,
            [0x4000_0002, 0x4000_0022, 0x4000_0023, 0x4000_0118]
        );
        for msr in ROUTED_MSRS {
            let intercept = msr_index_intercept(msr);
            assert_eq!(
                intercept.access_type_mask,
                HV_INTERCEPT_ACCESS_MASK_READ_WRITE
            );
            assert_eq!(
                intercept.intercept_type,
                mshv_bindings::hv_intercept_type_HV_INTERCEPT_TYPE_X64_MSR_INDEX
            );
            // SAFETY: every view of this C union is a plain integer.
            let parameter = unsafe { intercept.intercept_parameter.as_uint64 };
            assert_eq!(parameter, u64::from(msr));
        }
    }

    #[test]
    fn gp_fault_event_raises_gp_with_error_code_zero() {
        let event = hvdef::HvX64PendingExceptionEvent::from(gp_fault_event());
        assert!(event.event_pending());
        assert_eq!(event.event_type(), hvdef::HV_X64_PENDING_EVENT_EXCEPTION);
        assert_eq!(event.vector(), 13);
        assert!(event.deliver_error_code());
        assert_eq!(event.error_code(), 0);
    }

    #[test]
    fn partition_cpuid_registers_the_configured_identity_verbatim() {
        let own = vec![
            CpuidLeaf::new(0x4000_0000, [0x4000_0006, 1, 2, 3]),
            CpuidLeaf::new(0x4000_0081, [0x3123_5356, 0, 0, 0]),
            CpuidLeaf::new(0x4000_0100, [0x4000_0101, 0x4b4d_564b, 0x564b_4d56, 0x4d]),
            CpuidLeaf::new(0xd, [7, 0x340, 0x340, 0]).indexed(0),
        ];
        let config = time_abi_cpuid(VP_COUNT, true);
        let cpuid = partition_cpuid(own, &config);
        let result = |function| cpuid.result(function, 0, &[0; 4]);

        assert_eq!(
            result(0x4000_0000),
            [0x4000_0005, 0x7263_694d, 0x666f_736f, 0x7648_2074]
        );
        assert_eq!(result(0x4000_0001), [0x3123_7648, 0, 0, 0]);
        assert_eq!(result(0x4000_0002), [0x0058_564e, 0x0001_0000, 0, 0]);
        assert_eq!(result(0x4000_0003), [0x8860, 0, 0, 0x100]);
        assert_eq!(result(0x4000_0004), [0, 0xffff_ffff, 0, 0]);
        assert_eq!(result(0x4000_0005), [VP_COUNT, VP_COUNT, 0, 0]);
        // The backend's own hypervisor-range leaves are dropped: every
        // registered hypervisor-range leaf is a configured one, verbatim.
        let hypervisor: Vec<_> = cpuid
            .leaves()
            .iter()
            .filter(|leaf| HYPERVISOR_CPUID_RANGE.contains(&leaf.function))
            .collect();
        let configured: Vec<_> = config
            .leaves()
            .iter()
            .filter(|leaf| HYPERVISOR_CPUID_RANGE.contains(&leaf.function))
            .collect();
        assert_eq!(hypervisor.len(), configured.len());
        for (registered, configured) in hypervisor.iter().zip(&configured) {
            assert_eq!(
                (
                    registered.function,
                    registered.index,
                    registered.result,
                    registered.mask
                ),
                (
                    configured.function,
                    configured.index,
                    configured.result,
                    configured.mask
                )
            );
        }
        assert_eq!(result(0x4000_0081), [0; 4]);
        assert_eq!(result(0x4000_0100), [0; 4]);
        // Other leaves are kept.
        assert_eq!(cpuid.result(0xd, 0, &[0; 4]), [7, 0x340, 0x340, 0]);
    }

    #[test]
    fn partition_cpuid_applies_the_time_bits_last() {
        let own = vec![
            CpuidLeaf::new(
                1,
                [
                    0x0005_0657,
                    0x0080_0800,
                    0x7ffa_fbff | (1 << 24),
                    0xbfeb_fbff,
                ],
            ),
            CpuidLeaf::new(6, [0x77, 2, 9, 0]),
            CpuidLeaf::new(0x15, [2, 0xd4, 0x017d_7840, 0]),
        ];
        let cpuid = partition_cpuid(own, &time_abi_cpuid(VP_COUNT, true));
        let [eax, ebx, ecx, edx] = cpuid.result(1, 0, &[0; 4]);
        assert_eq!((eax, ebx), (0x0005_0657, 0x0080_0800));
        assert_eq!(ecx & (1 << 31), 1 << 31, "hypervisor present");
        assert_eq!(ecx & (1 << 24), 0, "TSC-deadline timer");
        assert_eq!(ecx & (1 << 15), 0, "PDCM");
        assert_eq!(edx, 0xbfeb_fbff);
        assert_eq!(cpuid.result(6, 0, &[!0; 4]), [4, 0, 0, 0]);
        assert_eq!(cpuid.result(0x15, 0, &[!0; 4]), [0; 4]);
        assert_eq!(cpuid.result(0x8000_0007, 0, &[!0; 4]), [0, 0, 0, 0x100]);
    }

    #[test]
    fn zero_fill_leaves_are_full_zero_leaves_past_the_identity() {
        for function in [0x4000_0006, 0x4000_0081, 0x4000_00ff] {
            assert!(is_zero_fill(&CpuidLeaf::new(function, [0; 4])));
        }
        // Identity leaves, nonzero or partial leaves, and zeroed time-bit
        // leaves outside the range are read back from VP 0.
        assert!(!is_zero_fill(&CpuidLeaf::new(0x4000_0004, [0; 4])));
        assert!(!is_zero_fill(&CpuidLeaf::new(0x4000_0006, [1, 0, 0, 0])));
        assert!(!is_zero_fill(&CpuidLeaf::new(0x4000_0100, [0; 4])));
        assert!(!is_zero_fill(&CpuidLeaf::new(0x15, [0; 4])));
        assert!(!is_zero_fill(
            &CpuidLeaf::new(0x4000_0006, [0; 4]).masked([1, 0, 0, 0])
        ));
    }

    #[test]
    fn time_abi_bank1_keeps_invariant_tsc_and_hides_the_other_time_features() {
        let supported = super::super::supported_processor_features1();
        assert!(supported.tsc_deadline_tmr_support());
        assert!(supported.tsc_adjust_support());
        assert!(supported.a_count_m_count_support());
        let features = processor_features1(supported);
        assert!(features.tsc_invariant_support());
        assert!(!features.tsc_deadline_tmr_support());
        assert!(!features.tsc_adjust_support());
        assert!(!features.a_count_m_count_support());
        // Nothing else changes.
        let time_bits = hvdef::HvX64PartitionProcessorFeatures1::new()
            .with_tsc_invariant_support(true)
            .with_tsc_deadline_tmr_support(true)
            .with_tsc_adjust_support(true)
            .with_a_count_m_count_support(true);
        let others = !u64::from(time_bits);
        assert_eq!(u64::from(features) & others, u64::from(supported) & others);

        let args = with_feature_banks(
            super::super::partition_create_args(&virt::ProtoPartitionIsolation::None, false, false)
                .unwrap(),
        );
        let banks = args.pt_cpu_fbanks;
        assert_eq!(banks[1], !u64::from(features));
        assert_eq!(
            banks[0],
            !u64::from(super::super::supported_processor_features())
        );
    }

    #[test]
    fn time_abi_rejects_isolation_and_the_hyperv_interface() {
        validate_partition(virt::IsolationType::None, false).unwrap();
        for (isolation, hv) in [
            (virt::IsolationType::Snp, false),
            (virt::IsolationType::None, true),
        ] {
            assert_eq!(
                validate_partition(isolation, hv).unwrap_err().code,
                TimeAbiCode::IdentityRouting
            );
        }
    }

    #[test]
    fn created_vps_skip_an_uncreated_suffix() {
        let vps = |created: &[bool]| -> Vec<u32> {
            created_vps(created.iter().copied())
                .into_iter()
                .map(|vp| vp.index())
                .collect()
        };
        assert_eq!(vps(&[true]), [0]);
        assert_eq!(
            vps(&[true, true, true, true, false, false, false, false]),
            [0, 1, 2, 3]
        );
        assert_eq!(vps(&[false, false]), Vec::<u32>::new());
    }

    #[test]
    fn readback_must_equal_the_target_on_every_vp() {
        let target = 0x1234_5678_9abc;
        let readback: Vec<_> = (0..4).map(|vp| (VpIndex::new(vp), target)).collect();
        check_readback(target, &readback).unwrap();
        check_readback(target, &[]).unwrap();

        let mut skewed = readback.clone();
        skewed[2].1 += 1;
        let error = check_readback(target, &skewed).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::TscSyncReadback);
        assert!(error.message.contains("VP 2"), "{error}");
    }

    #[test]
    fn late_vp_creation_fails_after_the_synchronized_set() {
        let config = TimeAbiConfig {
            cpuid: Arc::new(time_abi_cpuid(VP_COUNT, true)),
            msrs: Arc::new(TimeAbiMsrs::new()),
        };
        let state = MshvTimeAbi::new(&config, &partition_cpuid(Vec::new(), &config.cpuid));
        state.check_vp_creation(VpIndex::new(4)).unwrap();
        state.vp_set_sealed.store(true, Ordering::SeqCst);
        let error = state.check_vp_creation(VpIndex::new(4)).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::VpLateCreation);
        assert_eq!(
            error.to_string(),
            "[E_VP_LATE_CREATION] VP 4 was instantiated after the synchronized TSC set"
        );
    }

    fn sample(utc_ns: u64, monotonic_ns: u64) -> HostTimeSample {
        HostTimeSample {
            utc_ns,
            monotonic_ns,
        }
    }

    #[test]
    fn capture_anchor_pairs_with_the_bracket_midpoint() {
        let candidate = AnchorCandidate {
            tsc: 42,
            before: sample(1_000_000, 500),
            after: sample(1_003_001, 3_501),
        };
        assert_eq!(candidate.width_ns(), 3_001);
        assert_eq!(
            candidate.anchor(),
            TscAnchor {
                tsc: 42,
                sample: sample(1_001_500, 2_000),
                pairing_ns: 1_501,
            }
        );
    }

    #[test]
    fn capture_anchor_ignores_a_utc_step_inside_the_bracket() {
        let candidate = AnchorCandidate {
            tsc: 7,
            before: sample(5_000_000, 100),
            after: sample(1_000, 2_100),
        };
        assert_eq!(candidate.anchor().sample, sample(5_001_000, 1_100));
    }

    #[test]
    fn capture_anchor_selects_the_tightest_bracket_within_the_bound() {
        let candidate = |tsc, width| AnchorCandidate {
            tsc,
            before: sample(0, 0),
            after: sample(width, width),
        };
        let anchor = select_anchor(&[
            candidate(1, 30_000),
            candidate(2, 8_000),
            candidate(3, 12_000),
        ])
        .unwrap();
        assert_eq!((anchor.tsc, anchor.pairing_ns), (2, 4_000));

        // The bound applies to the pairing: half the bracket.
        assert_eq!(
            select_anchor(&[candidate(4, 20_000)]).unwrap().pairing_ns,
            10_000
        );
        let error = select_anchor(&[candidate(5, 20_001), candidate(6, 50_000)]).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::TscAnchor);
        assert_eq!(select_anchor(&[]).unwrap_err().code, TimeAbiCode::TscAnchor);
    }

    #[test]
    fn error_chain_includes_sources() {
        let error = KernelError::Hypercall {
            code: hvdef::HypercallCode::HvCallInstallIntercept,
            error: hvdef::HvError::AccessDenied,
        };
        let text = error_chain(&error);
        assert!(text.starts_with("hypercall"), "{text}");
        assert!(text.contains(": "), "{text}");
    }
}

/// Hardware tests. Run them on an MSHV root partition with
/// `cargo test -p virt_mshv -- --ignored --nocapture --test-threads=1 time_abi::hw`.
#[cfg(test)]
mod hw {
    use super::*;
    use crate::LinuxMshv;
    use guestmem::GuestMemory;
    use hvdef::Vtl;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use virt::BindProcessor;
    use virt::Hypervisor;
    use virt::Partition;
    use virt::PartitionConfig;
    use virt::PartitionMemoryMapper;
    use virt::ProtoPartition;
    use virt::ProtoPartitionConfig;
    use virt::time_abi::identity::check_identity;
    use virt::time_abi::identity::check_time_bits;
    use virt::time_abi::identity::time_abi_cpuid;
    use virt::time_abi::rate::LAPIC_HZ_HYPERV;
    use virt::time_abi::rate::check_plausible_tsc_hz;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    const VP_CAPACITY: u32 = 4;

    fn percentile(sorted: &[u64], percent: usize) -> u64 {
        sorted[(sorted.len() - 1) * percent / 100]
    }

    /// Builds a time ABI partition, binds the first `created` VPs, and runs
    /// every backend primitive against the hypervisor.
    #[async_test]
    #[ignore = "requires /dev/mshv"]
    async fn time_abi_backend_on_hardware(driver: DefaultDriver) {
        let processor_topology = TopologyBuilder::new_x86()
            .x2apic(X2ApicState::Supported)
            .build(VP_CAPACITY)
            .unwrap();
        let mem_layout = MemoryLayout::new(0x400000, &[], &[], &[], None).unwrap();
        let vmtime_keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vmtime = vmtime_keeper.builder().build(&driver).await.unwrap();

        for created in [1, 2, VP_CAPACITY] {
            let guest_memory = GuestMemory::allocate(mem_layout.end_of_ram() as usize);
            let mut mshv = LinuxMshv::new().unwrap();
            let started = std::time::Instant::now();
            let (partition, mut binders) = mshv
                .new_partition(ProtoPartitionConfig {
                    processor_topology: &processor_topology,
                    hv_config: None,
                    vmtime: &vmtime,
                    isolation: virt::ProtoPartitionIsolation::None,
                    nested_virt: false,
                    user_mode_memory_faults: false,
                    lazy_memory_registration: false,
                    versioned_cpu_contract: false,
                    time_abi: Some(TimeAbiConfig {
                        cpuid: Arc::new(time_abi_cpuid(VP_CAPACITY, true)),
                        msrs: Arc::new(TimeAbiMsrs::new()),
                    }),
                })
                .unwrap()
                .build(PartitionConfig {
                    mem_layout: &mem_layout,
                    guest_memory: &guest_memory,
                    cpuid: &[],
                    vtl0_alias_map: None,
                    fault_resolver: None,
                })
                .unwrap();
            let build_us = started.elapsed().as_micros();
            let ram = guest_memory.inner_buf().unwrap();
            // SAFETY: the guest memory outlives the partition.
            unsafe {
                partition.memory_mapper(Vtl::Vtl0).map_range(
                    ram.as_ptr().cast_mut().cast(),
                    ram.len(),
                    0,
                    true,
                    true,
                )
            }
            .unwrap();
            partition.finalize_memory().unwrap();
            for binder in &mut binders[..created as usize] {
                binder.bind().unwrap();
            }

            assert!(!partition.caps().hv1);
            assert!(!partition.caps().kvm_clock);
            let backend = partition.time_abi().expect("a time ABI partition");

            let started = std::time::Instant::now();
            let preflight = backend.preflight().unwrap();
            let preflight_us = started.elapsed().as_micros();
            assert_eq!(
                preflight,
                BackendPreflight {
                    msr_route: IdentityMsrRoute::ExitToVmm,
                    sync: TscSyncMethod::FrozenWrite,
                }
            );
            let tsc_hz = backend.native_tsc_hz().unwrap();
            check_plausible_tsc_hz(tsc_hz).unwrap();
            assert_eq!(backend.lapic_hz().unwrap(), LAPIC_HZ_HYPERV);

            let started = std::time::Instant::now();
            let effective = CpuidLeafSet::new(backend.effective_cpuid().unwrap());
            let effective_us = started.elapsed().as_micros();
            let mut lookup = |leaf, subleaf| effective.result(leaf, subleaf, &[0; 4]);
            check_time_bits(&mut lookup, true).unwrap();
            check_identity(&mut lookup, VP_CAPACITY).unwrap();

            let mut pairings: Vec<u64> = (0..200)
                .map(|_| backend.capture_anchor().unwrap().pairing_ns)
                .collect();
            pairings.sort_unstable();
            let first = backend.capture_anchor().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            let second = backend.capture_anchor().unwrap();
            let elapsed_ns = second.sample.monotonic_ns - first.sample.monotonic_ns;
            let measured_hz =
                u128::from(second.tsc - first.tsc) * 1_000_000_000 / u128::from(elapsed_ns);
            let deviation_ppm = (measured_hz as f64 - tsc_hz as f64) / tsc_hz as f64 * 1_000_000.0;
            assert!(
                deviation_ppm.abs() < 200.0,
                "anchors measure {measured_hz} Hz against {tsc_hz} Hz"
            );

            let target = 1_000_000_000_000;
            let mut anchor = None;
            let started = std::time::Instant::now();
            let report = backend
                .set_synchronized_tsc(&mut |sample| {
                    anchor = Some(*sample);
                    Ok(target)
                })
                .unwrap();
            let set_us = started.elapsed().as_micros();
            assert_eq!(report.target, target);
            assert_eq!(Some(report.sample), anchor);
            assert_eq!(report.method, TscSyncMethod::FrozenWrite);
            let expected: Vec<_> = (0..created).map(|vp| (VpIndex::new(vp), target)).collect();
            assert_eq!(report.readback, expected);

            // Time stays frozen until a VP runs.
            std::thread::sleep(std::time::Duration::from_millis(10));
            for vp in 0..created {
                assert_eq!(
                    get_vp_tsc(&partition.inner.vmfd, VpIndex::new(vp)).unwrap(),
                    target
                );
            }
            if created < VP_CAPACITY {
                let error = binders[created as usize].bind().err().unwrap();
                assert!(
                    error.to_string().starts_with("[E_VP_LATE_CREATION]"),
                    "{error}"
                );
            }

            // A failing target aborts the set.
            let error = backend
                .set_synchronized_tsc(&mut |_| {
                    Err(TimeAbiError::new(TimeAbiCode::DowntimeNegative, "test"))
                })
                .unwrap_err();
            assert_eq!(error.code, TimeAbiCode::DowntimeNegative);

            // After the thaw, every VP advances from the target together.
            partition.inner.thaw_time().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
            let after: Vec<u64> = (0..created)
                .map(|vp| get_vp_tsc(&partition.inner.vmfd, VpIndex::new(vp)).unwrap())
                .collect();
            assert!(after.iter().all(|&tsc| tsc > target + tsc_hz / 1000));

            println!(
                "time ABI backend: created {created}/{VP_CAPACITY} tsc_hz={tsc_hz} \
                 build_us={build_us} preflight_us={preflight_us} effective_cpuid_us={effective_us} \
                 ({} leaves) set_us={set_us} anchor_pairing_ns min={} p50={} p99={} max={} \
                 anchor_rate_deviation_ppm={deviation_ppm:.3} thawed={after:?}",
                effective.leaves().len(),
                pairings[0],
                percentile(&pairings, 50),
                percentile(&pairings, 99),
                pairings[pairings.len() - 1],
            );
        }
    }

    /// The negative path of identity routing: the hypervisor refuses an
    /// MSR-index intercept for `IA32_ARCH_CAPABILITIES` (AccessDenied), which
    /// must surface as `E_IDENTITY_ROUTING` naming the MSR.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn refused_msr_intercept_fails_identity_routing() {
        let args = with_feature_banks(
            super::super::partition_create_args(&virt::ProtoPartitionIsolation::None, false, false)
                .unwrap(),
        );
        let mshv = mshv_ioctls::Mshv::new().unwrap();
        let vmfd = crate::create_vm_with_retry(&mshv, &args).unwrap();
        vmfd.initialize().unwrap();
        route_identity_msrs(&vmfd).unwrap();
        let error = install_msr_intercepts(&vmfd, &[0x10a]).unwrap_err();
        println!("refused intercept: {error}");
        assert_eq!(error.code, TimeAbiCode::IdentityRouting);
        assert!(error.message.contains("MSR 0x10a"), "{error}");
    }

    /// Reports whether the processor feature banks can expose `BHI_CTRL`
    /// (CPUID.(EAX=7,ECX=2):EDX[4]) to a guest: Linux treats a guest with it
    /// as not vulnerable to indirect target selection.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn bhi_ctrl_exposure() {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap();
        let line = |prefix: &str| {
            cpuinfo
                .lines()
                .find(|line| line.starts_with(prefix))
                .unwrap_or_default()
        };
        let has = |line: &str, token: &str| line.split_whitespace().any(|word| word == token);
        println!(
            "host: {}; root flags bhi_ctrl={} bugs its={}",
            line("model name"),
            has(line("flags"), "bhi_ctrl"),
            has(line("bugs"), "its")
        );
        let mshv = mshv_ioctls::Mshv::new().unwrap();
        match mshv.get_host_partition_property(HvPartitionPropertyCode::ProcessorFeatures1.0) {
            Ok(bank1) => {
                let bank1 = hvdef::HvX64PartitionProcessorFeatures1::from(bank1);
                println!(
                    "host ProcessorFeatures1={:#018x}: bhi_dis_support={} bhi_no_support={}",
                    u64::from(bank1),
                    bank1.bhi_dis_support(),
                    bank1.bhi_no_support()
                );
            }
            Err(error) => println!("host ProcessorFeatures1 unavailable: {error}"),
        }
        let supported = super::super::supported_processor_features1();
        for (name, features1) in [
            ("default", supported),
            ("time-abi", processor_features1(supported)),
            (
                "bhi_dis",
                processor_features1(supported).with_bhi_dis_support(true),
            ),
            (
                "bhi_dis+bhi_no",
                processor_features1(supported)
                    .with_bhi_dis_support(true)
                    .with_bhi_no_support(true),
            ),
        ] {
            let mut args = super::super::partition_create_args(
                &virt::ProtoPartitionIsolation::None,
                false,
                false,
            )
            .unwrap();
            let mut banks = args.pt_cpu_fbanks;
            banks[1] = !u64::from(features1);
            args.pt_cpu_fbanks = banks;
            let vmfd = crate::create_vm_with_retry(&mshv, &args).unwrap();
            vmfd.initialize().unwrap();
            let vp = vmfd.create_vcpu(0).unwrap();
            let leaf7 = vp.get_cpuid_values(7, 0, 0, 0).unwrap();
            let leaf7_2 = vp.get_cpuid_values(7, 2, 0, 0).unwrap();
            println!(
                "banks={name}: CPUID.7.0={leaf7:#010x?} CPUID.7.2={leaf7_2:#010x?} \
                 BHI_CTRL={}",
                leaf7[0] >= 2 && leaf7_2[3] & (1 << 4) != 0
            );
        }
    }
}
