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
//!   Under an AMD CPU profile, intercepts also route HWCR and DE_CFG, which
//!   OpenVMM serves with the time ABI's fixed values ([`AMD_CONFIG_MSRS`]);
//!   no AMD MSHV host has verified them yet.
//! - CPUID intercept results present the configured CPUID verbatim: it is the
//!   CPU profile's complete effective CPUID, and the backend adds nothing to
//!   it. The per-VP topology leaves (the extended topology leaves and AMD's
//!   `0x8000001E`) are registered once per VP with that VP's APIC identity,
//!   which the hypervisor does not provide for these partitions. Entries the
//!   effective CPUID does not list show the hypervisor's own guest view,
//!   which reads zero there: the CPUID sweep hardware test checks it, and
//!   verification (`E_CPU_UNLISTED`) checks the ones the host enumerates at
//!   every boot and restore. The hypervisor reports no hypervisor leaf of its
//!   own, which preflight verifies at sentinel leaves.
//! - The processor feature banks follow the CPU profile (see
//!   [`profile_features`](super::profile_features)), keep the invariant TSC,
//!   and hide the TSC-deadline timer, `IA32_TSC_ADJUST`, and APERF/MPERF.
//! - The synchronized TSC set freezes partition time, writes the target to
//!   every created VP, reads it back, and resumes partition time right after a
//!   good read-back. Guest time therefore runs from the restore anchor, as on
//!   the other backends, instead of waiting for the first VP run.
//!
//! These semantics were verified on hypervisor builds 10.0.26100.30000 (bare
//! metal) and 10.0.26100.9444 (Azure nested), where they are identical.

use crate::KernelError;
use crate::MshvPartition;
use crate::MshvPartitionInner;
use crate::MshvProcessor;
use crate::VcpuFdExt;
use cpu_profile::CpuProfile;
use cpu_profile::CpuVendor;
use cpu_profile::PartitionProfile;
use cpu_profile::hv_banks::HvFeatures;
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
use virt::time_abi::identity::IDENTITY_CPUID_RANGE;
use virt::time_abi::identity::IDENTITY_MAX_LEAF;
use virt::time_abi::msr::AMD_CONFIG_MSRS;
use virt::time_abi::msr::MSR_APIC_FREQUENCY;
use virt::time_abi::msr::MSR_TSC_FREQUENCY;
use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
use virt::time_abi::msr::MSR_VP_INDEX;
use virt::time_abi::msr::amd_config_msr_read;
use virt::time_abi::msr::amd_config_msr_write;
use virt::time_abi::surface::SupportedCpuSurface;

/// The identity MSRs that OpenVMM serves. The hypervisor raises #GP for
/// every other MSR of the identity range.
pub(crate) const ROUTED_MSRS: [u32; 4] = [
    MSR_VP_INDEX,
    MSR_TSC_FREQUENCY,
    MSR_APIC_FREQUENCY,
    MSR_TSC_INVARIANT_CONTROL,
];

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
    /// The time ABI CPUID. Its partition-wide results are registered where
    /// VP 0's own view differs (see [`Self::register_cpuid`]), and its
    /// extended topology leaves for each VP when it is created.
    #[inspect(skip)]
    cpuid: Vec<CpuidLeaf>,
    /// The effective CPUID: VP 0's view of the time ABI CPUID's leaves at
    /// reset, and of the host's entries outside the CPU profile's tables,
    /// read once before any VP runs. Later reads would fold in VP state (the
    /// hypervisor applies the VP's XCR0, XSS, and control registers), so the
    /// capture and the restore of one snapshot would disagree.
    #[inspect(skip)]
    effective_cpuid: std::sync::OnceLock<Vec<CpuidLeaf>>,
    /// Set by the synchronized TSC set. VP creation fails afterwards.
    #[inspect(with = "|x| x.load(Ordering::Relaxed)")]
    vp_set_sealed: AtomicBool,
    /// The processor features the host partition offered at creation, for
    /// the supported CPU surface.
    #[inspect(skip)]
    host_features: HvFeatures,
    /// The CPU profile, whose unlisted candidates the host's CPUID gives.
    #[inspect(skip)]
    cpu_profile: PartitionProfile,
    /// Where the host's CPUID table comes from. After its first use, this
    /// holds only the drained channel, if any, which the partition frees when
    /// it drops.
    #[inspect(skip)]
    host_cpuid_source: parking_lot::Mutex<Option<HostCpuidSource>>,
    /// The host's CPUID table and the CPU profile's unlisted candidates in
    /// it, from [`Self::host_cpuid_source`] at first use.
    #[inspect(skip)]
    host_cpuid: std::sync::OnceLock<HostCpuid>,
    /// How long partition build took to start reading the host's CPUID, in
    /// microseconds.
    #[inspect(skip)]
    host_cpuid_spawn_us: u64,
}

/// The host's CPUID table, for the supported CPU surface and the unlisted
/// candidates.
pub(super) enum HostCpuidSource {
    /// Already read.
    Ready(HostCpuid),
    /// Being read on another thread, which sends the table with its unlisted
    /// candidates and how long it took, in microseconds. Every CPUID
    /// instruction in the root partition exits to the hypervisor, so this
    /// keeps the read off the start path: its first use comes after guest
    /// memory registration.
    Pending(std::sync::mpsc::Receiver<(HostCpuid, u64)>),
}

impl HostCpuidSource {
    /// Starts reading the host's CPUID table and finding `cpu_profile`'s
    /// unlisted candidates in it on another thread, or does both here if no
    /// thread can start.
    pub(super) fn spawn(cpu_profile: &PartitionProfile) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let thread_profile = cpu_profile.clone();
        // The thread is detached, so it releases its own stack when it exits
        // instead of a join on the start path.
        let read = move || {
            let started = std::time::Instant::now();
            let host = HostCpuid::read(&thread_profile);
            let _ = sender.send((host, started.elapsed().as_micros() as u64));
        };
        match std::thread::Builder::new()
            .name("mshv-host-cpuid".into())
            .spawn(read)
        {
            Ok(_) => Self::Pending(receiver),
            Err(error) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "cannot start a thread to read the host CPUID, reading it now"
                );
                Self::Ready(HostCpuid::read(cpu_profile))
            }
        }
    }
}

/// The host's CPUID table and the CPU profile's unlisted candidates in it.
pub(super) struct HostCpuid {
    table: Vec<cpu_profile::cpuid::CpuidEntry>,
    /// The entries the host's CPUID enumerates outside the CPU profile's
    /// tables (`cpu_profile::unlisted_cpuid_candidates`). The effective CPUID
    /// reports VP 0's view there, which core requires to be zero
    /// (`E_CPU_UNLISTED`).
    unlisted_candidates: Vec<(u32, Option<u32>)>,
    /// How long the other thread took to read the table and find the
    /// candidates, in microseconds, if it did.
    read_us: Option<u64>,
    /// How long the first use waited for them, in microseconds. The
    /// effective CPUID's log line reports both, so the first use logs
    /// nothing of its own: a separate event cost about 25 us on the start
    /// path.
    wait_us: u64,
}

impl HostCpuid {
    /// Finds `cpu_profile`'s unlisted candidates in `table`.
    fn new(cpu_profile: &CpuProfile, table: Vec<cpu_profile::cpuid::CpuidEntry>) -> Self {
        let unlisted_candidates = cpu_profile::unlisted_cpuid_candidates(cpu_profile, &table);
        Self {
            table,
            unlisted_candidates,
            read_us: None,
            wait_us: 0,
        }
    }

    /// Reads the host's CPUID table on this thread.
    fn read(cpu_profile: &CpuProfile) -> Self {
        Self::new(cpu_profile, super::profile_features::host_cpuid_table())
    }
}

impl MshvTimeAbi {
    pub(super) fn new(
        config: &TimeAbiConfig,
        cpuid: &CpuidLeafSet,
        host_features: HvFeatures,
        host_cpuid: HostCpuidSource,
        host_cpuid_spawn_us: u64,
    ) -> Self {
        Self {
            msrs: config.msrs.clone(),
            cpuid: cpuid.leaves().to_vec(),
            effective_cpuid: std::sync::OnceLock::new(),
            vp_set_sealed: AtomicBool::new(false),
            host_features,
            cpu_profile: config.cpu_profile.clone(),
            host_cpuid_source: parking_lot::Mutex::new(Some(host_cpuid)),
            host_cpuid: std::sync::OnceLock::new(),
            host_cpuid_spawn_us,
        }
    }

    /// Registers the partition-wide results of the time ABI CPUID that VP 0's
    /// own view does not already present, once VP 0 exists and before any VP
    /// runs or the effective CPUID is read.
    ///
    /// Under the CPU profile's processor features the hypervisor's own view
    /// presents most of the profile, and every hypervisor-range leaf past the
    /// identity reads zero. A result it already presents would change nothing
    /// the guest reads but cost a hypercall (about 3.3 us on bare metal and
    /// 5 us nested), so one bulk read of VP 0's view at reset decides. The
    /// effective CPUID report reads every leaf back, and core checks it. If
    /// the bulk read fails, every result is registered. The extended topology
    /// leaves get per-VP results when each VP is created (see `create_vp`).
    pub(super) fn register_cpuid(
        &self,
        vmfd: &VmFd,
        bsp: &VcpuFd,
        cpuid: &CpuidLeafSet,
    ) -> Result<(), crate::Error> {
        let started = std::time::Instant::now();
        let leaves: Vec<&CpuidLeaf> = cpuid
            .leaves()
            .iter()
            .filter(|leaf| !is_per_vp_leaf(leaf.function))
            .collect();
        let entries: Vec<(u32, u32)> = leaves
            .iter()
            .map(|leaf| (leaf.function, leaf.index.unwrap_or(0)))
            .collect();
        let native = vp_cpuid_many(bsp, VpIndex::BSP.index(), &entries)
            .inspect_err(|error| {
                tracing::warn!(
                    error = error as &dyn std::error::Error,
                    "MSHV bulk CPUID read failed, registering every time ABI CPUID result"
                );
            })
            .ok();
        let read_us = started.elapsed().as_micros() as u64;
        let mut registered = 0;
        for (index, leaf) in leaves.iter().enumerate() {
            if native
                .as_ref()
                .is_some_and(|native| presents(leaf, native[index]))
            {
                continue;
            }
            super::register_cpuid_result(vmfd, leaf)?;
            registered += 1;
        }
        tracing::info!(
            leaves = cpuid.leaves().len(),
            per_vp_leaves = cpuid.leaves().len() - leaves.len(),
            native_leaves = leaves.len() - registered,
            registered_leaves = registered,
            host_cpuid_spawn_us = self.host_cpuid_spawn_us,
            read_us,
            elapsed_us = started.elapsed().as_micros() as u64,
            "registered MSHV CPUID results"
        );
        Ok(())
    }

    /// Returns the host's CPUID table and the unlisted candidates, waiting
    /// for the read that partition creation started if it is still running.
    fn host_cpuid(&self) -> &HostCpuid {
        self.host_cpuid.get_or_init(|| {
            let started = std::time::Instant::now();
            let mut source = self.host_cpuid_source.lock();
            let (mut host, read_us) = match source.take() {
                Some(HostCpuidSource::Ready(host)) => (host, None),
                Some(HostCpuidSource::Pending(receiver)) => {
                    let received = receiver.recv();
                    // Freeing the channel can hand its memory back to the
                    // kernel, which took about 12 us on a bare-metal host,
                    // so the channel lives until the partition drops.
                    *source = Some(HostCpuidSource::Pending(receiver));
                    match received {
                        Ok((host, read_us)) => (host, Some(read_us)),
                        // The reading thread panicked.
                        Err(_) => (HostCpuid::read(&self.cpu_profile), None),
                    }
                }
                None => (HostCpuid::read(&self.cpu_profile), None),
            };
            drop(source);
            host.read_us = read_us;
            host.wait_us = started.elapsed().as_micros() as u64;
            host
        })
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

/// Returns the processor features of a time ABI partition with CPU profile
/// `profile` on a host whose partition offers `host`: the profile's, within
/// what the host offers, under the time ABI's policy (see
/// [`time_abi_features`](super::profile_features::time_abi_features)). Fails
/// with `E_PROFILE_UNSUPPORTED`.
pub(super) fn partition_features(
    profile: &CpuProfile,
    host: HvFeatures,
) -> Result<HvFeatures, TimeAbiError> {
    let features = super::profile_features::time_abi_features(profile, host)?;
    tracing::info!(
        cpu_profile = profile.id(),
        bank0 = features.banks[0],
        bank1 = features.banks[1],
        xsave = features.xsave,
        host_bank0 = host.banks[0],
        host_bank1 = host.banks[1],
        host_xsave = host.xsave,
        "MSHV processor features from the CPU profile"
    );
    Ok(features)
}

/// Applies `features` to the partition creation arguments, whose banks hold
/// the disabled features.
pub(super) fn with_features(
    mut args: mshv_bindings::mshv_create_partition_v2,
    features: HvFeatures,
) -> mshv_bindings::mshv_create_partition_v2 {
    args.pt_cpu_fbanks = [!features.banks[0], !features.banks[1]];
    args.pt_disabled_xsave = !features.xsave;
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

/// Returns the MSRs that a time ABI partition with a CPU profile of `vendor`
/// routes to OpenVMM: the identity MSRs it serves, and under an AMD profile
/// AMD's configuration MSRs, whose fixed values OpenVMM serves too
/// ([`AMD_CONFIG_MSRS`]).
pub(super) fn routed_msrs(vendor: CpuVendor) -> Vec<u32> {
    let mut msrs = ROUTED_MSRS.to_vec();
    if vendor == CpuVendor::Amd {
        msrs.extend(AMD_CONFIG_MSRS.iter().map(|&(msr, _)| msr));
    }
    msrs
}

/// Routes the time ABI's MSRs of a CPU profile of `vendor` to OpenVMM
/// ([`routed_msrs`]). Fails with `E_IDENTITY_ROUTING`, naming the MSR whose
/// intercept the hypervisor refused.
pub(super) fn route_identity_msrs(vmfd: &VmFd, vendor: CpuVendor) -> Result<(), TimeAbiError> {
    install_msr_intercepts(vmfd, &routed_msrs(vendor))
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

/// Returns the CPUID table of a time ABI partition: the time ABI CPUID
/// verbatim.
///
/// It is the CPU profile's complete effective CPUID (the profile's leaves,
/// OpenVMM's topology leaves and APIC mode, and the identity and explicit zero
/// leaves), so neither the worker's leaves nor the backend's own add a leaf or
/// a bit. The partition registers its results where VP 0's own view differs
/// ([`MshvTimeAbi::register_cpuid`]): without synthetic processor features the
/// hypervisor reports every hypervisor-range leaf past the identity as zero,
/// and under the profile's processor features it presents most of the
/// profile. The hypervisor's own values also show where a result's mask is
/// clear (each VP's APIC identity, the runtime XSAVE sizes) and where no
/// result applies: the reserved entries, which its guest view reads as zero.
pub(super) fn partition_cpuid(config: &CpuidLeafSet) -> CpuidLeafSet {
    CpuidLeafSet::new(config.leaves().to_vec())
}

/// Returns whether CPUID leaf `function` carries a field that differs per VP
/// and that the hypervisor does not provide for a time ABI partition: the
/// extended topology leaves `0xB` and `0x1F`, whose `EDX` is the VP's x2APIC
/// ID, and AMD's processor topology leaf `0x8000001E`, whose extended APIC,
/// compute unit, and node IDs are the VP's. The hypervisor does not
/// implement the extended topology leaves for these partitions and reads
/// that `EDX` as 0 on every VP, so each VP gets its own results; without
/// `0x8000001E`'s, a guest that takes its APIC ID from there would log
/// `APIC ID mismatch` on every AP. Leaf 1's initial APIC ID, the other
/// per-VP field, is the hypervisor's own.
pub(super) fn is_per_vp_leaf(function: u32) -> bool {
    matches!(function, 0xb | 0x1f | 0x8000_001e)
}

/// Returns `leaf` for the VP with APIC ID `apic_id` in a partition with
/// `reserved_vps_per_socket` APIC IDs per socket, its per-VP fields set as
/// `virt::x86::topology` sets the BSP's: the x2APIC ID in the extended
/// topology leaves' `EDX`, and in `0x8000001E` the extended APIC ID, the
/// compute unit ID within the socket, and the socket's node ID.
pub(super) fn with_vp_identity(
    leaf: &CpuidLeaf,
    apic_id: u32,
    reserved_vps_per_socket: u32,
) -> CpuidLeaf {
    let mut leaf = *leaf;
    if leaf.function == 0x8000_001e {
        let ebx = x86defs::cpuid::ProcessorTopologyDefinitionEbx::from(leaf.result[1]);
        let threads_per_compute_unit = u32::from(ebx.threads_per_compute_unit()) + 1;
        let reserved_vps_per_socket = reserved_vps_per_socket.max(1);
        leaf.result[0] = apic_id;
        leaf.result[1] = ebx
            .with_compute_unit_id(
                (apic_id % reserved_vps_per_socket / threads_per_compute_unit) as u8,
            )
            .into();
        leaf.result[2] = x86defs::cpuid::ProcessorTopologyDefinitionEcx::from(leaf.result[2])
            .with_node_id((apic_id / reserved_vps_per_socket) as u8)
            .into();
    } else {
        leaf.result[3] = apic_id;
    }
    let per_vp = virt::x86::topology::per_vp_cpuid_bits(leaf.function);
    for (mask, per_vp) in leaf.mask.iter_mut().zip(per_vp) {
        *mask |= per_vp;
    }
    leaf
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

/// Serves an intercepted access of a partition whose CPU profile is of
/// `vendor`: under an AMD profile, AMD's configuration MSRs with their fixed
/// values, and otherwise the identity MSRs through their handler.
fn serve_time_abi_msr(
    msrs: &TimeAbiMsrs,
    vendor: CpuVendor,
    vp: VpIndex,
    msr: u32,
    access: MsrAccess,
) -> MsrOutcome {
    let amd = || match access {
        MsrAccess::Read => amd_config_msr_read(msr).map(|value| Ok(MsrOutcome::Read(value))),
        MsrAccess::Write(value) => {
            amd_config_msr_write(msr, value).map(|r| r.map(|()| MsrOutcome::Written))
        }
    };
    let identity = || match access {
        MsrAccess::Read => msrs.read(vp, msr).map(|r| r.map(MsrOutcome::Read)),
        MsrAccess::Write(value) => msrs
            .write(vp, msr, value)
            .map(|r| r.map(|()| MsrOutcome::Written)),
    };
    let outcome = if vendor == CpuVendor::Amd {
        amd().or_else(identity)
    } else {
        identity()
    };
    match outcome {
        Some(Ok(outcome)) => outcome,
        Some(Err(_)) => MsrOutcome::Fault,
        None => {
            // Only the routed MSRs are intercepted, so this is a hypervisor
            // defect; the access still faults as an unknown MSR would.
            tracelimit::error_ratelimited!(
                vp = vp.index(),
                msr,
                "MSR intercept outside the time ABI's routed MSRs"
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
    /// Completes an intercepted access to a time ABI MSR, or raises #GP.
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
        let outcome = serve_time_abi_msr(
            &time_abi.msrs,
            time_abi.cpu_profile.cpu_vendor(),
            self.vpindex,
            info.msr_number,
            access,
        );
        tracing::trace!(
            vp = self.vpindex.index(),
            msr = info.msr_number,
            ?access,
            ?outcome,
            "time ABI MSR access"
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
pub(super) fn error_chain(error: &dyn std::error::Error) -> String {
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

/// The most CPUID entries that one `HvCallGetVpCpuidValues` reads. Its input,
/// a header and one leaf description per entry, must fit in the page that the
/// kernel copies it to.
const CPUID_READS_PER_CALL: usize = 128;

/// The input of `HvCallGetVpCpuidValues` for up to [`CPUID_READS_PER_CALL`]
/// entries.
#[repr(C, packed)]
struct VpCpuidValuesInput {
    header: mshv_bindings::hv_input_get_vp_cpuid_values,
    leaves: [mshv_bindings::hv_cpuid_leaf_info; CPUID_READS_PER_CALL],
}

const _: () = assert!(size_of::<VpCpuidValuesInput>() <= 4096);

/// Reads CPUID `entries`, each a leaf and subleaf, as VP `vp_index` sees them
/// with the registered results applied, which is how
/// `VcpuFd::get_cpuid_values` reads one entry. `HvCallGetVpCpuidValues` is a
/// rep hypercall, so one call reads up to [`CPUID_READS_PER_CALL`] entries
/// instead of paying a hypercall per entry.
pub(super) fn vp_cpuid_many(
    vp: &VcpuFd,
    vp_index: u32,
    entries: &[(u32, u32)],
) -> Result<Vec<[u32; 4]>, KernelError> {
    use mshv_bindings::hv_cpuid_leaf_info;
    use mshv_bindings::hv_input_get_vp_cpuid_values;
    use mshv_bindings::hv_output_get_vp_cpuid_values;

    let mut input = VpCpuidValuesInput {
        header: hv_input_get_vp_cpuid_values {
            vp_index,
            ..Default::default()
        },
        leaves: [hv_cpuid_leaf_info::default(); CPUID_READS_PER_CALL],
    };
    // SAFETY: both fields of the flags union are the same 32 bits.
    unsafe {
        let flags = &mut input.header.flags.__bindgen_anon_1;
        flags.set_use_vp_xfem_xss(1);
        flags.set_apply_registered_values(1);
    }
    let mut output = [hv_output_get_vp_cpuid_values::default(); CPUID_READS_PER_CALL];
    let mut results = Vec::with_capacity(entries.len());
    let mut pending = entries;
    while !pending.is_empty() {
        let count = pending.len().min(CPUID_READS_PER_CALL);
        for (leaf, &(eax, ecx)) in input.leaves.iter_mut().zip(&pending[..count]) {
            *leaf = hv_cpuid_leaf_info {
                eax,
                ecx,
                xfem: 0,
                xss: 0,
            };
        }
        let mut args = mshv_bindings::mshv_root_hvcall {
            code: hvdef::HypercallCode::HvCallGetVpCpuidValues.0,
            reps: count as u16,
            in_sz: (size_of::<hv_input_get_vp_cpuid_values>()
                + count * size_of::<hv_cpuid_leaf_info>()) as u16,
            in_ptr: std::ptr::addr_of!(input) as u64,
            out_sz: (count * size_of::<hv_output_get_vp_cpuid_values>()) as u16,
            out_ptr: output.as_mut_ptr() as u64,
            ..Default::default()
        };
        vp.hvcall(&mut args)?;
        // A call that completes only some of its entries is continued with
        // the rest.
        let completed = usize::from(args.reps);
        if completed == 0 || completed > count {
            return Err(KernelError::Kernel(std::io::Error::from_raw_os_error(
                libc::EINTR,
            )));
        }
        // SAFETY: both fields of the output union are the same four
        // registers.
        results.extend(
            output[..completed]
                .iter()
                .map(|values| unsafe { values.as_uint32 }),
        );
        pending = &pending[completed..];
    }
    Ok(results)
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

    /// Returns the effective CPUID, reading it from VP 0 the first time. The
    /// first read happens in preflight, before any VP runs, on both cold boot
    /// and restore, so it is VP 0's view at reset.
    fn time_abi_effective_cpuid(&self) -> Result<&[CpuidLeaf], TimeAbiError> {
        let state = self.time_abi_state()?;
        if let Some(leaves) = state.effective_cpuid.get() {
            return Ok(leaves);
        }
        let bsp = self.time_abi_bsp(TimeAbiCode::CpuSurface)?;
        let started = std::time::Instant::now();
        // VP 0 reads, in the report's order: the maximum leaves, the time ABI
        // CPUID's leaves, and the entries outside the profile's tables where
        // the host's CPUID has any. Guests read the hypervisor's own values
        // wherever no result was registered (see `register_cpuid`) and at the
        // unlisted entries (at subleaf 0 for a subleaf-independent entry),
        // which core requires to read zero.
        let reads: Vec<(u32, Option<u32>)> = EFFECTIVE_CPUID_MAX_LEAVES
            .into_iter()
            .map(|function| (function, None))
            .chain(state.cpuid.iter().map(|leaf| (leaf.function, leaf.index)))
            .chain(state.host_cpuid().unlisted_candidates.iter().copied())
            .collect();
        let entries: Vec<(u32, u32)> = reads
            .iter()
            .map(|&(function, index)| (function, index.unwrap_or(0)))
            .collect();
        let (values, bulk) = vp0_cpuid_entries(bsp, &entries, TimeAbiCode::CpuSurface)?;
        let leaves: Vec<_> = reads
            .iter()
            .zip(values)
            .map(|(&(function, index), result)| {
                let leaf = CpuidLeaf::new(function, result);
                match index {
                    Some(index) => leaf.indexed(index),
                    None => leaf,
                }
            })
            .collect();
        let host = state.host_cpuid();
        tracing::info!(
            leaves = leaves.len(),
            reads = entries.len(),
            bulk,
            unlisted_candidates = host.unlisted_candidates.len(),
            host_cpuid_entries = host.table.len(),
            host_cpuid_read_us = host.read_us,
            host_cpuid_wait_us = host.wait_us,
            elapsed_us = started.elapsed().as_micros() as u64,
            "MSHV effective CPUID read"
        );
        // A concurrent first read stores identical values.
        let _ = state
            .effective_cpuid
            .set(CpuidLeafSet::new(leaves).into_leaves());
        Ok(state.effective_cpuid.get().expect("set above"))
    }

    /// Checks that the effective CPUID holds the identity leaves as
    /// registered, and that VP 0 reads zero at the sentinel leaves.
    fn check_identity_cpuid(&self, bsp: &VcpuFd) -> Result<(), TimeAbiError> {
        let state = self.time_abi_state()?;
        let effective = CpuidLeafSet::new(self.time_abi_effective_cpuid()?.to_vec());
        let identity = state.cpuid.iter().filter(|leaf| {
            leaf.function <= IDENTITY_MAX_LEAF && IDENTITY_CPUID_RANGE.contains(&leaf.function)
        });
        for leaf in identity {
            let actual = effective.result(leaf.function, leaf.index.unwrap_or(0), &[0; 4]);
            check_leaf(leaf, actual)?;
        }
        let sentinels = PREFLIGHT_SENTINEL_LEAVES.map(|function| (function, 0));
        let (values, _) = vp0_cpuid_entries(bsp, &sentinels, TimeAbiCode::IdentityRouting)?;
        for (&(function, _), actual) in sentinels.iter().zip(values) {
            check_leaf(&CpuidLeaf::new(function, [0; 4]), actual)?;
        }
        Ok(())
    }
}

/// Reads CPUID `entries` of VP 0 in one bulk call ([`vp_cpuid_many`]),
/// falling back to one read per entry, with a warning, if the bulk call
/// fails. Returns the values and whether the bulk call served them; a read
/// that fails fails with `code`.
fn vp0_cpuid_entries(
    bsp: &VcpuFd,
    entries: &[(u32, u32)],
    code: TimeAbiCode,
) -> Result<(Vec<[u32; 4]>, bool), TimeAbiError> {
    match vp_cpuid_many(bsp, VpIndex::BSP.index(), entries) {
        Ok(values) => Ok((values, true)),
        Err(error) => {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "MSHV bulk CPUID read failed, reading one entry per call"
            );
            let values = entries
                .iter()
                .map(|&(function, index)| {
                    vp_cpuid(bsp, function, index).map_err(|error| {
                        TimeAbiError::new(
                            code,
                            format!(
                                "cannot read CPUID {function:#x}/{index:#x} of VP 0: {}",
                                error_chain(&error)
                            ),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((values, false))
        }
    }
}

/// Returns whether VP 0's `actual` result agrees with `leaf` under its mask.
fn presents(leaf: &CpuidLeaf, actual: [u32; 4]) -> bool {
    let mut expected = actual;
    leaf.apply(&mut expected);
    actual == expected
}

/// Fails with `E_IDENTITY_ROUTING` unless VP 0's `actual` result agrees with
/// `leaf` under its mask.
fn check_leaf(leaf: &CpuidLeaf, actual: [u32; 4]) -> Result<(), TimeAbiError> {
    if !presents(leaf, actual) {
        let mut expected = actual;
        leaf.apply(&mut expected);
        return Err(TimeAbiError::new(
            TimeAbiCode::IdentityRouting,
            format!(
                "VP 0 reads CPUID {:#x} as {actual:#x?}, expected {expected:#x?}",
                leaf.function
            ),
        ));
    }
    Ok(())
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

        // Freezing partition time checks that it can be frozen. Time stays
        // frozen, as after a reset, until the synchronized TSC set resumes it
        // after its read-back or a VP first runs, so a restore freezes it
        // only once.
        self.inner.freeze_time().map_err(|error| {
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
        Ok(self.inner.time_abi_effective_cpuid()?.to_vec())
    }

    fn supported_cpu_surface(&self) -> Result<Option<SupportedCpuSurface>, TimeAbiError> {
        let started = std::time::Instant::now();
        let state = self.inner.time_abi_state()?;
        let width = self
            .inner
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::PhysicalAddressWidth.0)
            .map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::ProfileUnsupported,
                    format!(
                        "cannot read the PhysicalAddressWidth partition property: {}",
                        error_chain(&KernelError::from(error))
                    ),
                )
            })?;
        let surface = super::profile_features::supported_cpu_surface(
            state.host_cpuid().table.clone(),
            state.host_features,
            width as u8,
        );
        tracing::info!(
            leaves = surface.cpuid.len(),
            physical_address_width = surface.physical_address_width,
            elapsed_us = started.elapsed().as_micros() as u64,
            "MSHV supported CPU surface"
        );
        Ok(Some(surface))
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

        // Freeze partition time so that every VP holds the target from the
        // anchor until the read-back, then resume it right away.
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
        // A failed set leaves time frozen; the restore fails and tears the
        // partition down.
        let thawed = result.is_ok();
        if thawed {
            self.inner.thaw_time().map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::TscSyncUnsupported,
                    format!("cannot resume partition time: {}", error_chain(&error)),
                )
            })?;
        }
        tracing::info!(
            created_vps = created.len(),
            vp_capacity = self.inner.vps.len(),
            target,
            readback_equal = result.is_ok(),
            thawed,
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

    /// Serves an intercepted access under an Intel profile, which routes the
    /// identity MSRs only.
    fn serve_identity_msr(
        msrs: &TimeAbiMsrs,
        vp: VpIndex,
        msr: u32,
        access: MsrAccess,
    ) -> MsrOutcome {
        serve_time_abi_msr(msrs, CpuVendor::Intel, vp, msr, access)
    }

    /// An AMD profile also routes HWCR and DE_CFG, which OpenVMM serves with
    /// the time ABI's fixed values.
    #[test]
    fn amd_profiles_route_and_serve_the_amd_config_msrs() {
        assert_eq!(routed_msrs(CpuVendor::Intel), ROUTED_MSRS);
        assert_eq!(
            routed_msrs(CpuVendor::Amd),
            [
                0x4000_0002,
                0x4000_0022,
                0x4000_0023,
                0x4000_0118,
                0xc001_0015,
                0xc001_1029
            ]
        );
        let msrs = declared_msrs();
        let serve =
            |msr, access| serve_time_abi_msr(&msrs, CpuVendor::Amd, VpIndex::new(2), msr, access);
        assert_eq!(
            serve(0xc001_0015, MsrAccess::Read),
            MsrOutcome::Read(1 << 24)
        );
        assert_eq!(
            serve(0xc001_1029, MsrAccess::Read),
            MsrOutcome::Read(1 << 1)
        );
        assert_eq!(
            serve(0xc001_0015, MsrAccess::Write(1 << 24)),
            MsrOutcome::Written
        );
        assert_eq!(
            serve(0xc001_0015, MsrAccess::Write(1 << 24 | 1 << 30)),
            MsrOutcome::Fault
        );
        assert_eq!(serve(0xc001_1029, MsrAccess::Write(0)), MsrOutcome::Fault);
        assert_eq!(serve(MSR_VP_INDEX, MsrAccess::Read), MsrOutcome::Read(2));
        // An Intel profile does not route them, so an intercept faults.
        assert_eq!(
            serve_identity_msr(&msrs, VpIndex::BSP, 0xc001_0015, MsrAccess::Read),
            MsrOutcome::Fault
        );
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
            // SAFETY: `msr_index_intercept` writes the union through
            // `as_uint64`, so all eight bytes are initialized.
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
    fn partition_cpuid_is_the_time_abi_cpuid_verbatim() {
        let config = time_abi_cpuid(VP_COUNT, true);
        let cpuid = partition_cpuid(&config);
        let leaves = |set: &CpuidLeafSet| -> Vec<_> {
            set.leaves()
                .iter()
                .map(|leaf| (leaf.function, leaf.index, leaf.result, leaf.mask))
                .collect()
        };
        assert_eq!(leaves(&cpuid), leaves(&config));

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
        assert_eq!(result(0x4000_0081), [0; 4]);
        let mut lookup = |leaf, subleaf| cpuid.result(leaf, subleaf, &[0; 4]);
        virt::time_abi::identity::check_identity(&mut lookup, VP_COUNT).unwrap();

        // The time bits are the configured ones, and the bits the table does
        // not mask are left to the hypervisor.
        let [_, _, ecx, _] = cpuid.result(1, 0, &[!0; 4]);
        assert_eq!(ecx & (1 << 31), 1 << 31, "hypervisor present");
        assert_eq!(ecx & (1 << 24), 0, "TSC-deadline timer");
        assert_eq!(ecx & (1 << 15), 0, "PDCM");
        assert_eq!(cpuid.result(6, 0, &[!0; 4]), [4, 0, 0, 0]);
        assert_eq!(cpuid.result(0x15, 0, &[!0; 4]), [0; 4]);
        assert_eq!(cpuid.result(0x8000_0007, 0, &[!0; 4]), [0, 0, 0, 0x100]);
    }

    #[test]
    fn per_vp_topology_leaves_take_each_vp_apic_identity() {
        for function in [0xb, 0x1f, 0x8000_001e] {
            assert!(is_per_vp_leaf(function), "{function:#x}");
        }
        // Leaf 1's initial APIC ID is the hypervisor's own per-VP value.
        for function in [0, 1, 4, 0xd, 0x4000_0000, 0x8000_001d] {
            assert!(!is_per_vp_leaf(function), "{function:#x}");
        }
        let level = CpuidLeaf::new(0xb, [1, 2, 0x100, 0])
            .indexed(0)
            .masked([!0, !0, !0, 0]);
        let leaf = with_vp_identity(&level, 5, 8);
        assert_eq!(
            (leaf.function, leaf.index, leaf.result, leaf.mask),
            (0xb, Some(0), [1, 2, 0x100, 5], [!0; 4])
        );

        // AMD's processor topology, as the partition table holds it: the
        // threads per compute unit and the nodes per processor, with the
        // per-VP IDs unmasked. With SMT, APIC IDs 2 and 3 share compute unit
        // 1; APIC ID 13 is compute unit 2 of socket 1's node.
        let amd =
            CpuidLeaf::new(0x8000_001e, [0, 0x100, 0, 0]).masked([0, 0xffff_ff00, 0xffff_ff00, !0]);
        for (apic_id, expected) in [
            (0, [0, 0x100, 0, 0]),
            (3, [3, 0x101, 0, 0]),
            (13, [13, 0x102, 1, 0]),
        ] {
            let leaf = with_vp_identity(&amd, apic_id, 8);
            assert_eq!(leaf.result, expected, "APIC ID {apic_id}");
            assert_eq!(leaf.mask, [!0; 4], "APIC ID {apic_id}");
        }
    }

    #[test]
    fn the_effective_cpuid_reads_the_host_entries_outside_the_profile() {
        let profile = cpu_profile::pinned("intel.skylake-sp.v1").unwrap();
        let entry = cpu_profile::cpuid::CpuidEntry::new;
        let mut host: Vec<_> = profile
            .cpuid()
            .iter()
            .map(|listed| {
                let (leaf, subleaf) = listed.key();
                entry(leaf, subleaf, listed.values())
            })
            .collect();
        // A cache past the profile's terminator and leaves past its maxima.
        host.push(entry(4, Some(5), [0x121, 0, 0, 0]));
        host.push(entry(0x17, None, [1, 0, 0, 0]));
        host.push(entry(0x8000_0009, None, [1, 0, 0, 0]));
        // The VM's topology and the identity range are not candidates.
        host.push(entry(0xb, Some(2), [0, 0, 2, 0]));
        host.push(entry(0x4000_0000, None, [0x4000_000b, 0, 0, 0]));
        host.sort_by_key(cpu_profile::cpuid::CpuidEntry::key);
        let config = TimeAbiConfig {
            cpuid: Arc::new(time_abi_cpuid(VP_COUNT, true)),
            msrs: Arc::new(TimeAbiMsrs::new()),
            cpu_profile: PartitionProfile::pinned(profile.id()).unwrap(),
        };
        let state = MshvTimeAbi::new(
            &config,
            &partition_cpuid(&config.cpuid),
            super::super::profile_features::legacy_features(),
            HostCpuidSource::Ready(HostCpuid::new(&config.cpu_profile, host)),
            0,
        );
        assert_eq!(
            state.host_cpuid().unlisted_candidates,
            [(4, Some(5)), (0x17, None), (0x8000_0009, None)]
        );
    }

    #[test]
    fn the_host_cpuid_is_read_in_the_background() {
        let config = TimeAbiConfig {
            cpuid: Arc::new(time_abi_cpuid(VP_COUNT, true)),
            msrs: Arc::new(TimeAbiMsrs::new()),
            cpu_profile: PartitionProfile::pinned("intel.skylake-sp.v1").unwrap(),
        };
        let state = MshvTimeAbi::new(
            &config,
            &partition_cpuid(&config.cpuid),
            super::super::profile_features::legacy_features(),
            HostCpuidSource::spawn(&config.cpu_profile),
            0,
        );
        let here = HostCpuid::read(&config.cpu_profile);
        // The reading thread may run on another CPU, whose per-CPU fields
        // (the APIC IDs) differ, so compare the entries it enumerates. The
        // candidates exclude the topology leaves, so they are the same.
        let keys = |table: &[cpu_profile::cpuid::CpuidEntry]| {
            table
                .iter()
                .map(cpu_profile::cpuid::CpuidEntry::key)
                .collect::<Vec<_>>()
        };
        let host = state.host_cpuid();
        assert!(host.read_us.is_some(), "read on another thread");
        assert_eq!(keys(&host.table), keys(&here.table));
        assert_eq!(host.unlisted_candidates, here.unlisted_candidates);
    }

    #[test]
    fn a_result_is_presented_when_the_view_agrees_under_its_mask() {
        let leaf = CpuidLeaf::new(6, [4, 0, 0, 0]).masked([!0, !0, 1, 0]);
        assert!(presents(&leaf, [4, 0, 0, 0]));
        // Bits outside the mask are the hypervisor's own.
        assert!(presents(&leaf, [4, 0, 2, 0x55]));
        assert!(!presents(&leaf, [0, 0, 0, 0]));
        assert!(!presents(&leaf, [4, 0, 1, 0]));
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

        let features = HvFeatures {
            banks: [0x1234, 0x5678],
            xsave: 0x3f,
        };
        let args = with_features(
            super::super::partition_create_args(&virt::ProtoPartitionIsolation::None, false, false)
                .unwrap(),
            features,
        );
        let banks = args.pt_cpu_fbanks;
        let disabled_xsave = args.pt_disabled_xsave;
        assert_eq!(banks, [!0x1234, !0x5678]);
        assert_eq!(disabled_xsave, !0x3f);
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
            cpu_profile: PartitionProfile::pinned("intel.skylake-sp.v1").unwrap(),
        };
        let state = MshvTimeAbi::new(
            &config,
            &partition_cpuid(&config.cpuid),
            super::super::profile_features::legacy_features(),
            HostCpuidSource::Ready(HostCpuid::new(&config.cpu_profile, Vec::new())),
            0,
        );
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
        let widest = 2 * MAX_ANCHOR_PAIRING_NS;
        assert_eq!(
            select_anchor(&[candidate(4, widest)]).unwrap().pairing_ns,
            MAX_ANCHOR_PAIRING_NS
        );
        let error =
            select_anchor(&[candidate(5, widest + 1), candidate(6, 5 * widest)]).unwrap_err();
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
    use virt::time_abi::rate::LAPIC_HZ_HYPERV;
    use virt::time_abi::rate::check_plausible_tsc_hz;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    const VP_CAPACITY: u32 = 4;

    /// Returns the pinned CPU profile of this host's generation.
    fn host_profile() -> PartitionProfile {
        let selected = cpu_profile::select_auto(&cpu_profile::HostCpuSignature::current()).unwrap();
        PartitionProfile::pinned(selected.id()).unwrap()
    }

    /// Returns the effective CPUID of this host's profile for `topology`, and
    /// the CPUID table that core programs for it (`TimeAbiConfig::cpuid`).
    fn host_profile_cpuid(
        topology: &vm_topology::processor::ProcessorTopology,
    ) -> (cpu_profile::EffectiveCpuid, CpuidLeafSet) {
        let effective = virt::time_abi::cpuid::effective_cpuid(&host_profile(), topology).unwrap();
        let table = virt::time_abi::cpuid::backend_cpuid(&effective);
        (effective, table)
    }
    /// Converts a backend's CPU surface to the profile crate's form, as core
    /// does for the support check.
    fn host_cpu_surface(surface: &SupportedCpuSurface) -> cpu_profile::HostCpuSurface {
        let mut cpuid = surface
            .cpuid
            .iter()
            .map(|leaf| cpu_profile::cpuid::CpuidEntry::new(leaf.function, leaf.index, leaf.result))
            .collect();
        cpu_profile::cpuid::normalize(&mut cpuid);
        cpu_profile::HostCpuSurface {
            cpuid,
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
        }
    }

    fn percentile(sorted: &[u64], percent: usize) -> u64 {
        sorted[(sorted.len() - 1) * percent / 100]
    }

    /// Whether `effective` lists CPUID `function` at subleaf `index`.
    fn is_listed(effective: &cpu_profile::EffectiveCpuid, function: u32, index: u32) -> bool {
        effective
            .results()
            .any(|result| result.function == function && result.index.is_none_or(|i| i == index))
    }

    /// The CPUID entries that a sweep of VP 0 probes for `effective`.
    ///
    /// - Near, where guests enumerate: subleaves 0 to 63 of every indexed
    ///   leaf, and the four leaves past each maximum.
    /// - Far, where no enumeration reaches, without the entries `effective`
    ///   lists: subleaves 64 to 255 of every indexed leaf, the leaves past the
    ///   near ones up to 0xff and 0x800000ff, the hypervisor range up to
    ///   0x400001ff, and a few distant leaves.
    fn sweep_probes(effective: &cpu_profile::EffectiveCpuid) -> (Vec<(u32, u32)>, Vec<(u32, u32)>) {
        let mut indexed: Vec<u32> = effective
            .results()
            .filter(|result| result.index.is_some())
            .map(|result| result.function)
            .collect();
        indexed.dedup();
        let max_basic = effective.lookup(0, 0)[0];
        let max_extended = effective.lookup(0x8000_0000, 0)[0];
        let near = indexed
            .iter()
            .flat_map(|&function| (0..64).map(move |index| (function, index)))
            .chain((max_basic + 1..=max_basic + 4).map(|function| (function, 0)))
            .chain((max_extended + 1..=max_extended + 4).map(|function| (function, 0)))
            .collect();
        let far = indexed
            .iter()
            .flat_map(|&function| (64..256).map(move |index| (function, index)))
            .chain((max_basic + 5..=0xff).map(|function| (function, 0)))
            .chain((max_extended + 5..=0x8000_00ff).map(|function| (function, 0)))
            .chain((0x4000_0000..=0x4000_01ff).map(|function| (function, 0)))
            .chain(
                [
                    0x100,
                    0x1000,
                    0x2000_0000,
                    0x3fff_ffff,
                    0x8000_0100,
                    0x8fff_ffff,
                    0xc000_0000,
                    0xc000_0001,
                    0xffff_ffff,
                ]
                .map(|function| (function, 0)),
            )
            .filter(|&(function, index)| !is_listed(effective, function, index))
            .collect();
        (near, far)
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
                    time_abi: Some(TimeAbiConfig {
                        cpuid: Arc::new(host_profile_cpuid(&processor_topology).1),
                        msrs: Arc::new(TimeAbiMsrs::new()),
                        cpu_profile: host_profile(),
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
            // Preflight leaves partition time frozen, as a reset does, until
            // the synchronized TSC set or the first VP run resumes it.
            assert!(*partition.inner.time_frozen.lock());
            partition.inner.thaw_time().unwrap();
            let tsc_hz = backend.native_tsc_hz().unwrap();
            check_plausible_tsc_hz(tsc_hz).unwrap();
            assert_eq!(backend.lapic_hz().unwrap(), LAPIC_HZ_HYPERV);

            // The supported surface is cheap and supports this host's profile.
            let started = std::time::Instant::now();
            let surface = backend
                .supported_cpu_surface()
                .unwrap()
                .expect("MSHV reports its CPU surface");
            let surface_us = started.elapsed().as_micros();
            let selected = host_profile();
            let profile = &*selected;
            cpu_profile::verify_support(profile, &host_cpu_surface(&surface)).unwrap();

            let started = std::time::Instant::now();
            let effective = CpuidLeafSet::new(backend.effective_cpuid().unwrap());
            let effective_us = started.elapsed().as_micros();
            let mut lookup = |leaf, subleaf| effective.result(leaf, subleaf, &[0; 4]);
            check_time_bits(&mut lookup, true).unwrap();
            check_identity(&mut lookup, VP_CAPACITY).unwrap();

            // The report holds VP 0's view at every entry the host's CPUID has
            // outside the profile's tables, and each reads zero.
            let candidates = cpu_profile::unlisted_cpuid_candidates(
                profile,
                &super::super::profile_features::host_cpuid_table(),
            );
            for &(function, index) in &candidates {
                assert!(
                    effective
                        .leaves()
                        .iter()
                        .any(|leaf| leaf.function == function && leaf.index == index),
                    "{function:#x} {index:?} is missing from the effective CPUID"
                );
            }
            let entries: Vec<_> = effective
                .leaves()
                .iter()
                .map(|leaf| {
                    cpu_profile::cpuid::CpuidEntry::new(leaf.function, leaf.index, leaf.result)
                })
                .collect();
            cpu_profile::check_unlisted_cpuid(profile, &entries).unwrap();
            println!(
                "time ABI backend: {} unlisted candidates read zero: {candidates:x?}",
                candidates.len()
            );

            // The hypervisor folds VP state into CPUID reads, so the effective
            // CPUID must stay VP 0's view at reset, whatever the guest does.
            let bsp = &partition.inner.finalized().unwrap().bsp_vcpufd;
            let live_before = bsp.get_cpuid_values(1, 0, 0, 0).unwrap();
            let mut cr4 = [HvRegisterAssoc::from((HvX64RegisterName::Cr4, 0u64))];
            bsp.get_hvdef_regs(&mut cr4).unwrap();
            let reset_cr4 = cr4[0].value.as_u64();
            let osxsave = bsp.set_hvdef_regs(&[HvRegisterAssoc::from((
                HvX64RegisterName::Cr4,
                reset_cr4 | (1 << 18),
            ))]);
            let live_after = bsp.get_cpuid_values(1, 0, 0, 0).unwrap();
            let key = |leaves: &[CpuidLeaf]| -> Vec<(u32, Option<u32>, [u32; 4])> {
                leaves
                    .iter()
                    .map(|l| (l.function, l.index, l.result))
                    .collect()
            };
            assert_eq!(
                key(&backend.effective_cpuid().unwrap()),
                key(effective.leaves())
            );
            bsp.set_hvdef_regs(&[HvRegisterAssoc::from((HvX64RegisterName::Cr4, reset_cr4))])
                .unwrap();
            let osxsave = format!(
                "cr4_osxsave_write={} live_cpuid1_ecx={:#x}->{:#x}",
                osxsave.is_ok(),
                live_before[2],
                live_after[2]
            );

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

            // Time resumes right after the read-back, and every VP advances
            // from the target together.
            assert!(!*partition.inner.time_frozen.lock());
            std::thread::sleep(std::time::Duration::from_millis(10));
            let running: Vec<u64> = (0..created)
                .map(|vp| get_vp_tsc(&partition.inner.vmfd, VpIndex::new(vp)).unwrap())
                .collect();
            assert!(
                running.iter().all(|&tsc| tsc > target + tsc_hz / 200),
                "{running:?}"
            );
            let spread = running.iter().max().unwrap() - running.iter().min().unwrap();
            assert!(spread < tsc_hz / 1000, "{running:?}");
            if created < VP_CAPACITY {
                let error = binders[created as usize].bind().err().unwrap();
                assert!(
                    error.to_string().starts_with("[E_VP_LATE_CREATION]"),
                    "{error}"
                );
            }

            // A failing target aborts the set and leaves time frozen.
            let error = backend
                .set_synchronized_tsc(&mut |_| {
                    Err(TimeAbiError::new(TimeAbiCode::DowntimeNegative, "test"))
                })
                .unwrap_err();
            assert_eq!(error.code, TimeAbiCode::DowntimeNegative);
            assert!(*partition.inner.time_frozen.lock());
            let frozen_at: Vec<u64> = (0..created)
                .map(|vp| get_vp_tsc(&partition.inner.vmfd, VpIndex::new(vp)).unwrap())
                .collect();
            std::thread::sleep(std::time::Duration::from_millis(10));
            for (vp, &tsc) in frozen_at.iter().enumerate() {
                assert_eq!(
                    get_vp_tsc(&partition.inner.vmfd, VpIndex::new(vp as u32)).unwrap(),
                    tsc
                );
            }

            // A later thaw (the first VP run) resumes every VP together.
            partition.inner.thaw_time().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
            let after: Vec<u64> = (0..created)
                .map(|vp| get_vp_tsc(&partition.inner.vmfd, VpIndex::new(vp)).unwrap())
                .collect();
            assert!(after.iter().all(|&tsc| tsc > running[0] + tsc_hz / 200));

            println!(
                "time ABI backend: created {created}/{VP_CAPACITY} tsc_hz={tsc_hz} \
                 build_us={build_us} preflight_us={preflight_us} effective_cpuid_us={effective_us} \
                 ({} leaves) surface_us={surface_us} ({} leaves, {}-bit) set_us={set_us} \
                 anchor_pairing_ns min={} p50={} p99={} max={} \
                 anchor_rate_deviation_ppm={deviation_ppm:.3} {osxsave} thawed={after:?}",
                effective.leaves().len(),
                surface.cpuid.len(),
                surface.physical_address_width,
                pairings[0],
                percentile(&pairings, 50),
                percentile(&pairings, 99),
                pairings[pairings.len() - 1],
            );
        }
    }

    /// Sweeps, as VP 0 of a time ABI partition observes them, subleaves 0 to
    /// 63 of every indexed leaf of the effective CPUID and the four leaves
    /// past the maximum basic and extended leaves. Listed results must match
    /// under their masks. A leaf or subleaf that the table does not list must
    /// read zero, which is the hypervisor's own guest view there (the backend
    /// registers no result for it), except past the extended topology leaves'
    /// terminators: those are reserved, and the sweep only reports them. One
    /// rep hypercall must read every probe as the single reads do, and the
    /// test times both ways. Farther out, where no guest enumeration reaches
    /// (subleaves 64 to 255, leaves up to 0xff and 0x800000ff, the hypervisor
    /// range up to 0x400001ff, and a few distant leaves), every unlisted entry
    /// must read zero as well (see `sweep_probes`). Last, a non-zero value
    /// planted at a reserved entry that the host enumerates must reach the
    /// backend's effective CPUID report and fail core's `E_CPU_UNLISTED`
    /// check.
    #[async_test]
    #[ignore = "requires /dev/mshv"]
    async fn unlisted_cpuid_entries_read_zero(driver: DefaultDriver) {
        let processor_topology = TopologyBuilder::new_x86()
            .x2apic(X2ApicState::Supported)
            .build(2)
            .unwrap();
        let (effective, table) = host_profile_cpuid(&processor_topology);
        let mem_layout = MemoryLayout::new(0x400000, &[], &[], &[], None).unwrap();
        let vmtime_keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vmtime = vmtime_keeper.builder().build(&driver).await.unwrap();
        let guest_memory = GuestMemory::allocate(mem_layout.end_of_ram() as usize);
        let mut mshv = LinuxMshv::new().unwrap();
        let (partition, mut binders) = mshv
            .new_partition(ProtoPartitionConfig {
                processor_topology: &processor_topology,
                hv_config: None,
                vmtime: &vmtime,
                isolation: virt::ProtoPartitionIsolation::None,
                nested_virt: false,
                user_mode_memory_faults: false,
                lazy_memory_registration: false,
                time_abi: Some(TimeAbiConfig {
                    cpuid: Arc::new(table),
                    msrs: Arc::new(TimeAbiMsrs::new()),
                    cpu_profile: host_profile(),
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
        binders[0].bind().unwrap();
        let bsp = &partition.inner.finalized().unwrap().bsp_vcpufd;

        let mut indexed: Vec<u32> = effective
            .results()
            .filter(|result| result.index.is_some())
            .map(|result| result.function)
            .collect();
        indexed.dedup();
        let max_basic = effective.lookup(0, 0)[0];
        let max_extended = effective.lookup(0x8000_0000, 0)[0];
        let (probes, far) = sweep_probes(&effective);
        let (mut count, mut listed_failures, mut unlisted, mut reserved) =
            (0, Vec::new(), Vec::new(), Vec::new());
        let mut single = Vec::with_capacity(probes.len());
        for &(function, index) in &probes {
            count += 1;
            let listed = effective.results().find(|result| {
                result.function == function && result.index.is_none_or(|i| i == index)
            });
            let (expected, mask) =
                listed.map_or(([0; 4], [!0; 4]), |result| (result.result, result.mask));
            let actual = bsp.get_cpuid_values(function, index, 0, 0).unwrap();
            single.push(actual);
            if (0..4).all(|register| (actual[register] ^ expected[register]) & mask[register] == 0)
            {
                continue;
            }
            let line = format!("{function:#x}.{index}: {actual:08x?}");
            if listed.is_some() {
                listed_failures.push(format!("{line} expected {expected:08x?} mask {mask:08x?}"));
            } else if is_per_vp_leaf(function) {
                reserved.push(line);
            } else {
                unlisted.push(line);
            }
        }
        println!(
            "{}: {count} probes of {} indexed leaves (max basic {max_basic:#x}, max extended {max_extended:#x}); {} listed results differ, {} unlisted results are not zero, {} reserved topology entries are not zero",
            host_profile().id(),
            indexed.len(),
            listed_failures.len(),
            unlisted.len(),
            reserved.len()
        );
        for line in listed_failures.iter().chain(&unlisted).chain(&reserved) {
            println!("  {line}");
        }
        assert!(listed_failures.is_empty(), "{listed_failures:#?}");
        assert!(unlisted.is_empty(), "{unlisted:#?}");

        // One rep hypercall reads the same values, registered results
        // included, over several calls when the probes exceed one call.
        assert_eq!(
            vp_cpuid_many(bsp, 0, &probes).unwrap(),
            single,
            "bulk and single CPUID reads differ"
        );
        // Time both ways over the entries that the effective CPUID report
        // reads, as `time_abi_effective_cpuid` does, and the bulk read with
        // the host's unlisted candidates as well.
        let report: Vec<(u32, u32)> = effective
            .results()
            .map(|result| (result.function, result.index.unwrap_or(0)))
            .collect();
        let profile = host_profile();
        let candidates = cpu_profile::unlisted_cpuid_candidates(
            &profile,
            &super::super::profile_features::host_cpuid_table(),
        );
        let with_candidates: Vec<(u32, u32)> = report
            .iter()
            .copied()
            .chain(
                candidates
                    .iter()
                    .map(|&(function, index)| (function, index.unwrap_or(0))),
            )
            .collect();
        let (mut single_us, mut bulk_us, mut candidates_us) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..11 {
            let started = std::time::Instant::now();
            for &(function, index) in &report {
                bsp.get_cpuid_values(function, index, 0, 0).unwrap();
            }
            single_us.push(started.elapsed().as_micros() as u64);
            let started = std::time::Instant::now();
            vp_cpuid_many(bsp, 0, &report).unwrap();
            bulk_us.push(started.elapsed().as_micros() as u64);
            let started = std::time::Instant::now();
            vp_cpuid_many(bsp, 0, &with_candidates).unwrap();
            candidates_us.push(started.elapsed().as_micros() as u64);
        }
        single_us.sort_unstable();
        bulk_us.sort_unstable();
        candidates_us.sort_unstable();
        println!(
            "bulk CPUID read: {} probes match single reads; {} entries in {} us one per call, {} us in one call, {} us with the {} unlisted candidates (p50 of 11)",
            probes.len(),
            report.len(),
            single_us[5],
            bulk_us[5],
            candidates_us[5],
            candidates.len()
        );

        // Farther out, every entry the effective CPUID does not list must read
        // zero too (see `sweep_probes`).
        let values = vp_cpuid_many(bsp, 0, &far).unwrap();
        let (mut far_unlisted, mut far_reserved) = (Vec::new(), Vec::new());
        for (&(function, index), actual) in far.iter().zip(&values) {
            if *actual == [0; 4] {
                continue;
            }
            let line = format!("{function:#x}.{index}: {actual:08x?}");
            if is_per_vp_leaf(function) {
                far_reserved.push(line);
            } else {
                far_unlisted.push(line);
            }
        }
        println!(
            "{} far probes: {} unlisted results are not zero, {} reserved topology entries are not zero",
            far.len(),
            far_unlisted.len(),
            far_reserved.len()
        );
        for line in far_unlisted.iter().chain(&far_reserved) {
            println!("  {line}");
        }
        assert!(far_unlisted.is_empty(), "{far_unlisted:#?}");

        // The negative path of verification step 6. A non-zero value at a
        // reserved entry that the host enumerates, planted here as a
        // registered result, must reach the backend's effective CPUID report,
        // which core then rejects with E_CPU_UNLISTED: the report reads the
        // hypervisor's view at the candidates and never synthesizes zero.
        // Nothing has read the report before, so its first read sees the plant.
        let &(function, index) = candidates
            .first()
            .expect("the host enumerates reserved entries");
        let planted = CpuidLeaf::new(function, [0x5a5a_5a5a, 0, 0, 0]);
        let planted = match index {
            Some(index) => planted.indexed(index),
            None => planted,
        };
        super::super::register_cpuid_result(&partition.inner.vmfd, &planted).unwrap();
        let backend_report = partition.inner.time_abi_effective_cpuid().unwrap();
        assert!(
            backend_report.iter().any(|leaf| leaf.function == function
                && leaf.index == index
                && leaf.result == planted.result),
            "the report lacks the planted {function:#x} {index:?}"
        );
        let entries: Vec<_> = backend_report
            .iter()
            .map(|leaf| cpu_profile::cpuid::CpuidEntry::new(leaf.function, leaf.index, leaf.result))
            .collect();
        let error = cpu_profile::check_unlisted_cpuid(&profile, &entries).unwrap_err();
        assert_eq!(
            error.code,
            cpu_profile::ProfileErrorCode::CpuUnlisted,
            "{error}"
        );
        println!("planted {function:#x}.{index:?} = {planted:x?}: {error}");
    }

    /// Every VP of a time ABI partition reads its own APIC identity in the
    /// per-VP topology leaves, which OpenVMM registers per VP because the
    /// hypervisor reads it as 0 on every VP: the x2APIC ID in `EDX` of the
    /// extended topology leaves, and, under an AMD profile, the extended
    /// APIC, compute unit, and node IDs of `0x8000001E`; and the table's
    /// other registers. Leaf 1's initial APIC ID is the hypervisor's own and
    /// must be the VP's too.
    #[async_test]
    #[ignore = "requires /dev/mshv"]
    async fn extended_topology_reports_each_vp_x2apic_id(driver: DefaultDriver) {
        const VPS: u32 = 4;
        let processor_topology = TopologyBuilder::new_x86()
            .x2apic(X2ApicState::Supported)
            .build(VPS)
            .unwrap();
        let (_, table) = host_profile_cpuid(&processor_topology);
        let topology_leaves: Vec<CpuidLeaf> = table
            .leaves()
            .iter()
            .filter(|leaf| is_per_vp_leaf(leaf.function))
            .copied()
            .collect();
        assert!(!topology_leaves.is_empty());
        let mem_layout = MemoryLayout::new(0x400000, &[], &[], &[], None).unwrap();
        let vmtime_keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vmtime = vmtime_keeper.builder().build(&driver).await.unwrap();
        let guest_memory = GuestMemory::allocate(mem_layout.end_of_ram() as usize);
        let mut mshv = LinuxMshv::new().unwrap();
        let (partition, mut binders) = mshv
            .new_partition(ProtoPartitionConfig {
                processor_topology: &processor_topology,
                hv_config: None,
                vmtime: &vmtime,
                isolation: virt::ProtoPartitionIsolation::None,
                nested_virt: false,
                user_mode_memory_faults: false,
                lazy_memory_registration: false,
                time_abi: Some(TimeAbiConfig {
                    cpuid: Arc::new(table),
                    msrs: Arc::new(TimeAbiMsrs::new()),
                    cpu_profile: host_profile(),
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
        for binder in &mut binders {
            binder.bind().unwrap();
        }
        let finalized = partition.inner.finalized().unwrap();
        let mut checked = 0;
        for (vp, binder) in processor_topology.vps_arch().zip(&binders) {
            let vcpufd = if vp.base.vp_index.is_bsp() {
                &finalized.bsp_vcpufd
            } else {
                binder.vcpufd.as_ref().unwrap()
            };
            for leaf in &topology_leaves {
                let index = leaf.index.unwrap_or(0);
                let actual = vcpufd.get_cpuid_values(leaf.function, index, 0, 0).unwrap();
                let expected = with_vp_identity(
                    leaf,
                    vp.apic_id,
                    processor_topology.reserved_vps_per_socket(),
                );
                assert!(
                    (0..4).all(|r| (actual[r] ^ expected.result[r]) & expected.mask[r] == 0),
                    "VP {} CPUID {:#x}.{index}: {actual:08x?}, expected {:08x?}",
                    vp.base.vp_index.index(),
                    leaf.function,
                    expected.result
                );
                checked += 1;
            }
            let [_, ebx, _, _] = vcpufd.get_cpuid_values(1, 0, 0, 0).unwrap();
            assert_eq!(
                ebx >> 24,
                vp.apic_id & 0xff,
                "VP {} initial APIC ID",
                vp.base.vp_index.index()
            );
        }
        println!(
            "{VPS} VPs with x2APIC IDs {:?}: {checked} extended topology entries carry each VP's ID, and leaf 1 its initial APIC ID",
            processor_topology
                .vps_arch()
                .map(|vp| vp.apic_id)
                .collect::<Vec<_>>()
        );
    }

    /// The negative path of identity routing: the hypervisor refuses an
    /// MSR-index intercept for `IA32_ARCH_CAPABILITIES` (AccessDenied), which
    /// must surface as `E_IDENTITY_ROUTING` naming the MSR.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn refused_msr_intercept_fails_identity_routing() {
        let mshv = mshv_ioctls::Mshv::new().unwrap();
        let features = partition_features(
            &host_profile(),
            super::super::profile_features::host_features(&mshv).unwrap(),
        )
        .unwrap();
        let args = with_features(
            super::super::partition_create_args(&virt::ProtoPartitionIsolation::None, false, false)
                .unwrap(),
            features,
        );
        let vmfd = crate::create_vm_with_retry(&mshv, &args).unwrap();
        vmfd.initialize().unwrap();
        route_identity_msrs(&vmfd, CpuVendor::Intel).unwrap();
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
