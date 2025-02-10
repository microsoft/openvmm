// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the Underhill hypervisor backend, which uses
//! `/dev/mshv_vtl` to interact with the Microsoft hypervisor while running in
//! VTL2.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]

mod devmsr;

cfg_if::cfg_if!(
    if #[cfg(target_arch = "x86_64")] { // xtask-fmt allow-target-arch sys-crate
        mod cvm_cpuid;
        pub use processor::snp::shared_pages_required_per_cpu as snp_shared_pages_required_per_cpu;
        pub use processor::snp::SnpBacked;
        pub use processor::tdx::shared_pages_required_per_cpu as tdx_shared_pages_required_per_cpu;
        pub use processor::tdx::TdxBacked;
        pub use crate::processor::mshv::x64::HypervisorBackedX86 as HypervisorBacked;
        use devmsr::MsrDevice;
        use hv1_emulator::hv::ProcessorVtlHv;
        use processor::snp::SnpBackedShared;
        use processor::tdx::TdxBackedShared;
        use std::arch::x86_64::CpuidResult;
        use virt::CpuidLeaf;
        use bitvec::prelude::BitArray;
        use bitvec::prelude::Lsb0;
        /// Bitarray type for representing IRR bits in a x86-64 APIC
        /// Each bit represent the 256 possible vectors.
        type IrrBitmap = BitArray<[u32; 8], Lsb0>;
    } else if #[cfg(target_arch = "aarch64")] { // xtask-fmt allow-target-arch sys-crate
        pub use crate::processor::mshv::arm64::HypervisorBackedArm64 as HypervisorBacked;
        use hvdef::HvArm64RegisterName;
    }
);

mod processor;
pub use processor::Backing;
pub use processor::UhProcessor;

use anyhow::Context as AnyhowContext;
use bitfield_struct::bitfield;
use bitvec::boxed::BitBox;
use bitvec::vec::BitVec;
use guestmem::GuestMemory;
use hcl::ioctl::Hcl;
use hcl::ioctl::SetVsmPartitionConfigError;
use hcl::GuestVtl;
use hv1_emulator::hv::GlobalHv;
use hv1_emulator::hv::VtlProtectHypercallOverlay;
use hv1_emulator::message_queues::MessageQueues;
use hv1_emulator::synic::GlobalSynic;
use hv1_emulator::synic::SintProxied;
use hv1_structs::VtlArray;
use hvdef::hypercall::HostVisibilityType;
use hvdef::hypercall::HvGuestOsId;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::HvInterceptParameters;
use hvdef::hypercall::HvInterceptType;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_EXECUTE;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_NONE;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_READ_WRITE;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_WRITE;
use hvdef::GuestCrashCtl;
use hvdef::HvAllArchRegisterName;
use hvdef::HvError;
use hvdef::HvMapGpaFlags;
use hvdef::HvRegisterName;
use hvdef::HvRegisterVsmPartitionConfig;
use hvdef::HvRegisterVsmPartitionStatus;
use hvdef::Vtl;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use pal::unix::affinity;
use pal::unix::affinity::CpuSet;
use pal_async::driver::Driver;
use pal_async::driver::SpawnDriver;
use pal_uring::IdleControl;
use parking_lot::Mutex;
use parking_lot::RwLock;
use processor::BackingSharedParams;
use processor::SidecarExitReason;
use sidecar_client::NewSidecarClientError;
use std::ops::RangeInclusive;
use std::os::fd::AsRawFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Weak;
use std::task::Waker;
use thiserror::Error;
use virt::irqcon::IoApicRouting;
use virt::irqcon::MsiRequest;
use virt::x86::apic_software_device::ApicSoftwareDevices;
use virt::CpuidLeafSet;
use virt::IsolationType;
use virt::PartitionCapabilities;
use virt::VpIndex;
use virt_support_apic::LocalApicSet;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::TargetVpInfo;
use vmcore::monitor::MonitorPage;
use vmcore::reference_time_source::ReferenceTimeSource;
use vmcore::vmtime::VmTimeSource;
use x86defs::snp::REG_TWEAK_BITMAP_OFFSET;
use x86defs::snp::REG_TWEAK_BITMAP_SIZE;
use x86defs::tdx::TdCallResult;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// General error returned by operations.
#[derive(Error, Debug)]
#[expect(missing_docs)]
pub enum Error {
    #[error("hcl error")]
    Hcl(#[source] hcl::ioctl::Error),
    #[error("failed to open sidecar client")]
    Sidecar(#[source] NewSidecarClientError),
    #[error("failed to install {0:?} intercept: {1:?}")]
    InstallIntercept(HvInterceptType, HvError),
    #[error("failed to query hypervisor register {0:#x?}")]
    Register(HvRegisterName, #[source] HvError),
    #[error("failed to set vsm partition config register")]
    VsmPartitionConfig(#[source] SetVsmPartitionConfigError),
    #[error("failed to create virtual device")]
    NewDevice(#[source] virt::x86::apic_software_device::DeviceIdInUse),
    #[error("failed to create cpuid tables for cvm")]
    #[cfg(guest_arch = "x86_64")]
    CvmCpuid(#[source] cvm_cpuid::CpuidResultsError),
    #[error("failed to update hypercall msr")]
    UpdateHypercallMsr,
    #[error("failed to update reference tsc msr")]
    UpdateReferenceTsc,
    #[error("failed to map overlay page")]
    MapOverlay(#[source] std::io::Error),
    #[error("failed to allocate shared visibility pages for overlay")]
    AllocateSharedVisOverlay(#[source] page_pool_alloc::Error),
    #[error("failed to open msr device")]
    OpenMsr(#[source] std::io::Error),
    #[error("cpuid did not contain valid TSC frequency information")]
    BadCpuidTsc,
    #[error("failed to read tsc frequency")]
    ReadTscFrequency(#[source] std::io::Error),
    #[error("tsc frequency mismatch between hypervisor ({hv}) and hardware {hw}, exceeds allowed error {allowed_error}")]
    TscFrequencyMismatch {
        hv: u64,
        hw: u64,
        allowed_error: u64,
    },
    #[error("failed to set vsm partition config: {0:?}")]
    FailedToSetL2Ctls(TdCallResult),
    #[error("debugging is configured but the binary does not have the gdb feature")]
    InvalidDebugConfiguration,
    #[error("missing shared memory for an isolated partition")]
    MissingSharedMemory,
}

/// Error revoking guest VSM.
#[derive(Error, Debug)]
#[expect(missing_docs)]
pub enum RevokeGuestVsmError {
    #[error("failed to set vsm config")]
    SetGuestVsmConfig(#[source] hcl::ioctl::SetGuestVsmConfigError),
    #[error("VTL 1 is already enabled")]
    Vtl1AlreadyEnabled,
}

/// Underhill partition.
#[derive(Inspect)]
pub struct UhPartition {
    #[inspect(flatten)]
    inner: Arc<UhPartitionInner>,
    // TODO: remove this extra indirection by refactoring some traits.
    #[inspect(skip)]
    interrupt_targets: VtlArray<Arc<UhInterruptTarget>, 2>,
}

#[derive(Inspect)]
#[inspect(extra = "UhPartitionInner::inspect_extra")]
struct UhPartitionInner {
    #[inspect(skip)]
    hcl: Hcl,
    #[inspect(skip)] // inspected separately
    vps: Vec<UhVpInner>,
    irq_routes: virt::irqcon::IrqRoutes,
    caps: PartitionCapabilities,
    #[inspect(skip)] // handled in `inspect_extra`
    enter_modes: Mutex<EnterModes>,
    #[inspect(skip)]
    enter_modes_atomic: AtomicU8,
    cpuid: Mutex<CpuidLeafSet>,
    lower_vtl_memory_layout: MemoryLayout,
    gm: VtlArray<GuestMemory, 2>,
    shared_memory: Option<GuestMemory>,
    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    #[inspect(skip)]
    crash_notification_send: mesh::Sender<VtlCrash>,
    monitor_page: MonitorPage,
    software_devices: Option<ApicSoftwareDevices>,
    /// The emulated local APIC set. This is only present for
    /// hardware-isolated VMs.
    lapic: Option<VtlArray<LocalApicSet, 2>>,
    #[inspect(skip)]
    vmtime: VmTimeSource,
    isolation: IsolationType,
    hide_isolation: bool,
    /// The emulated hypervisor state. This is only present for
    /// hardware-isolated VMs.
    hv: Option<GlobalHv>,
    /// The synic state used for untrusted SINTs, that is, the SINTs for which
    /// the guest thinks it is interacting directly with the untrusted
    /// hypervisor via an architecture-specific interface.
    ///
    /// This is only set for TDX VMs. For SNP VMs, this is implemented by the
    /// hypervisor. For non-isolated VMs, this isn't a concept.
    untrusted_synic: Option<GlobalSynic>,
    guest_vsm: RwLock<GuestVsmState>,
    #[inspect(skip)]
    isolated_memory_protector: Option<Arc<dyn ProtectIsolatedMemory>>,
    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    #[inspect(skip)]
    shared_vis_pages_pool: Option<page_pool_alloc::PagePoolAllocator>,
    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    #[inspect(skip)]
    private_vis_pages_pool: Option<page_pool_alloc::PagePoolAllocator>,
    #[inspect(with = "inspect::AtomicMut")]
    no_sidecar_hotplug: AtomicBool,
    use_mmio_hypercalls: bool,
    backing_shared: BackingShared,
    intercept_debug_exceptions: bool,
    #[cfg(guest_arch = "x86_64")]
    // N.B For now, only one device vector table i.e. for VTL0 only
    #[inspect(with = "|x| inspect::iter_by_index(x.read().into_inner().map(inspect::AsHex))")]
    device_vector_table: RwLock<IrrBitmap>,
}

#[derive(Inspect)]
#[inspect(external_tag)]
#[doc(hidden)]
pub enum BackingShared {
    // No shared state for hypervisor-backed VMs today.
    Hypervisor,
    #[cfg(guest_arch = "x86_64")]
    Snp(#[inspect(flatten)] SnpBackedShared),
    #[cfg(guest_arch = "x86_64")]
    Tdx(#[inspect(flatten)] TdxBackedShared),
}

impl BackingShared {
    fn new(
        isolation: IsolationType,
        backing_shared_params: BackingSharedParams,
    ) -> Result<BackingShared, Error> {
        Ok(match isolation {
            IsolationType::None | IsolationType::Vbs => {
                #[allow(irrefutable_let_patterns)]
                let BackingSharedParams {
                    cvm_state: None, ..
                } = backing_shared_params
                else {
                    unreachable!()
                };
                BackingShared::Hypervisor
            }
            #[cfg(guest_arch = "x86_64")]
            IsolationType::Snp => BackingShared::Snp(SnpBackedShared::new(backing_shared_params)?),
            #[cfg(guest_arch = "x86_64")]
            IsolationType::Tdx => BackingShared::Tdx(TdxBackedShared::new(backing_shared_params)?),
            #[cfg(not(guest_arch = "x86_64"))]
            _ => unreachable!(),
        })
    }
}

#[derive(InspectMut, Copy, Clone)]
struct EnterModes {
    #[inspect(mut)]
    first: EnterMode,
    #[inspect(mut)]
    second: EnterMode,
}

impl Default for EnterModes {
    fn default() -> Self {
        Self {
            first: EnterMode::Fast,
            second: EnterMode::IdleToVtl0,
        }
    }
}

impl From<EnterModes> for hcl::protocol::EnterModes {
    fn from(value: EnterModes) -> Self {
        Self::new()
            .with_first(value.first.into())
            .with_second(value.second.into())
    }
}

#[derive(InspectMut, Copy, Clone)]
enum EnterMode {
    Fast,
    PlayIdle,
    IdleToVtl0,
}

impl From<EnterMode> for hcl::protocol::EnterMode {
    fn from(value: EnterMode) -> Self {
        match value {
            EnterMode::Fast => Self::FAST,
            EnterMode::PlayIdle => Self::PLAY_IDLE,
            EnterMode::IdleToVtl0 => Self::IDLE_TO_VTL0,
        }
    }
}

#[cfg(guest_arch = "x86_64")]
#[derive(Inspect)]
/// VP state for CVMs.
pub struct UhCvmVpState {
    /// The VTLs on this VP waiting for TLB locks on other VPs.
    vtls_tlb_waiting: VtlArray<bool, 2>,
    /// Used in VTL 2 exit code to determine which VTL to exit to.
    exit_vtl: GuestVtl,
    /// Hypervisor enlightenment emulator state.
    hv: VtlArray<ProcessorVtlHv, 2>,
    /// LAPIC state.
    lapics: VtlArray<processor::LapicState, 2>,
}

#[cfg(guest_arch = "x86_64")]
impl UhCvmVpState {
    /// Creates a new CVM VP state.
    pub fn new(
        hv: VtlArray<ProcessorVtlHv, 2>,
        lapics: VtlArray<processor::LapicState, 2>,
    ) -> Self {
        Self {
            vtls_tlb_waiting: VtlArray::new(false),
            exit_vtl: GuestVtl::Vtl0,
            hv,
            lapics,
        }
    }
}

#[cfg(guest_arch = "x86_64")]
#[derive(Inspect)]
/// Partition-wide state for CVMs.
pub struct UhCvmPartitionState {
    #[inspect(skip)]
    cpuid: cvm_cpuid::CpuidResults,
    /// VPs that have locked their TLB.
    #[inspect(
        with = "|arr| inspect::iter_by_index(arr.iter()).map_value(|bb| inspect::iter_by_index(bb.iter().map(|v| *v)))"
    )]
    tlb_locked_vps: VtlArray<BitBox<AtomicU64>, 2>,
    /// The current status of TLB locks, per-VP.
    #[inspect(
        with = "|vec| inspect::iter_by_index(vec.iter().map(|arr| inspect::iter_by_index(arr.iter())))"
    )]
    tlb_lock_info: Vec<VtlArray<TlbLockInfo, 2>>,
    shared_memory: GuestMemory,
}

/// Partition-wide state for CVMs.
#[cfg(guest_arch = "aarch64")]
#[derive(Inspect)]
pub enum UhCvmPartitionState {}

#[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
#[derive(Inspect)]
#[inspect(tag = "guest vsm state")]
/// Partition-wide state for guest vsm. Only applies to CVMs.
enum GuestVsmState {
    NotPlatformSupported,
    NotGuestEnabled,
    Enabled { vtl1: GuestVsmVtl1State },
}

impl GuestVsmState {
    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    fn get_vbs_isolated(&self) -> Option<&VbsIsolatedVtl1State> {
        match self {
            GuestVsmState::Enabled {
                vtl1: GuestVsmVtl1State::VbsIsolated { state },
            } => Some(state),

            _ => None,
        }
    }

    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    fn get_vbs_isolated_mut(&mut self) -> Option<&mut VbsIsolatedVtl1State> {
        match self {
            GuestVsmState::Enabled {
                vtl1: GuestVsmVtl1State::VbsIsolated { state },
            } => Some(state),

            _ => None,
        }
    }

    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    fn get_hardware_cvm_mut(&mut self) -> Option<&mut HardwareCvmVtl1State> {
        match self {
            GuestVsmState::Enabled {
                vtl1: GuestVsmVtl1State::HardwareCvm { state },
            } => Some(state),

            _ => None,
        }
    }

    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    fn get_hardware_cvm(&self) -> Option<&HardwareCvmVtl1State> {
        match self {
            GuestVsmState::Enabled {
                vtl1: GuestVsmVtl1State::HardwareCvm { state },
            } => Some(state),
            _ => None,
        }
    }
}

#[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
#[derive(Clone, Copy, Inspect)]
#[inspect(external_tag)]
enum GuestVsmVtl1State {
    HardwareCvm { state: HardwareCvmVtl1State },
    VbsIsolated { state: VbsIsolatedVtl1State },
}

#[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
#[derive(Clone, Copy, Default, Inspect)]
struct VbsIsolatedVtl1State {
    #[inspect(with = "|flags| flags.map(|f| inspect::AsHex(u32::from(f)))")]
    default_vtl_protections: Option<HvMapGpaFlags>,
    enable_vtl_protection: bool,
}

#[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
#[derive(Clone, Copy, Default, Inspect)]
struct HardwareCvmVtl1State {
    /// Whether VTL 1 has been enabled on any vp
    enabled_on_vp_count: u32,
    /// Whether guest memory should be zeroed before it resets.
    zero_memory_on_reset: bool,
    /// Whether a vp can be started or reset by a lower vtl.
    deny_lower_vtl_startup: bool,
    /// Whether Mode-Based Execution Control should be enforced on lower VTLs.
    pub mbec_enabled: bool,
    /// Whether shadow supervisor stack is enabled.
    pub shadow_supervisor_stack_enabled: bool,
}

#[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
struct TscReferenceTimeSource {
    tsc_scale: u64,
}

#[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
impl TscReferenceTimeSource {
    fn new(tsc_frequency: u64) -> Self {
        TscReferenceTimeSource {
            tsc_scale: (((10_000_000_u128) << 64) / tsc_frequency as u128) as u64,
        }
    }
}

/// A time implementation based on TSC.
impl ReferenceTimeSource for TscReferenceTimeSource {
    fn now_100ns(&self) -> u64 {
        #[cfg(guest_arch = "x86_64")]
        {
            let tsc = safe_intrinsics::rdtsc();
            ((self.tsc_scale as u128 * tsc as u128) >> 64) as u64
        }

        #[cfg(guest_arch = "aarch64")]
        {
            todo!("AARCH64_TODO");
        }
    }

    fn is_backed_by_tsc(&self) -> bool {
        true
    }
}

#[cfg(guest_arch = "aarch64")]
impl virt::irqcon::ControlGic for UhPartitionInner {
    fn set_spi_irq(&self, irq_id: u32, high: bool) {
        if let Err(err) = self.hcl.request_interrupt(
            hvdef::HvInterruptControl::new()
                .with_arm64_asserted(high)
                .with_interrupt_type(hvdef::HvInterruptType::HvArm64InterruptTypeFixed),
            0,
            irq_id,
            GuestVtl::Vtl0,
        ) {
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                irq = irq_id,
                asserted = high,
                "failed to request spi"
            );
        }
    }
}

#[cfg(guest_arch = "aarch64")]
impl virt::Aarch64Partition for UhPartition {
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic> {
        debug_assert!(vtl == Vtl::Vtl0);
        self.inner.clone()
    }
}

/// A wrapper around [`UhProcessor`] that is [`Send`].
///
/// This is used to instantiate the processor object on the correct thread,
/// since all lower VTL processor state accesses must occur from the same
/// processor at VTL2.
pub struct UhProcessorBox {
    partition: Arc<UhPartitionInner>,
    vp_info: TargetVpInfo,
}

impl UhProcessorBox {
    /// Returns the VP index.
    pub fn vp_index(&self) -> VpIndex {
        self.vp_info.base.vp_index
    }

    /// Returns the base CPU that manages this processor, when it is a sidecar
    /// VP.
    pub fn sidecar_base_cpu(&self) -> Option<u32> {
        self.partition
            .hcl
            .sidecar_base_cpu(self.vp_info.base.vp_index.index())
    }

    /// Returns the processor object, bound to this thread.
    ///
    /// If `control` is provided, then this must be called on the VP's
    /// associated thread pool thread, and it will dispatch the VP directly.
    /// Otherwise, the processor will control the processor via the sidecar
    /// kernel.
    pub fn bind_processor<'a, T: Backing>(
        &'a mut self,
        driver: &impl Driver,
        control: Option<&'a mut IdleControl>,
    ) -> Result<UhProcessor<'a, T>, Error> {
        if let Some(control) = &control {
            let vp_index = self.vp_info.base.vp_index;

            let mut current = Default::default();
            affinity::get_current_thread_affinity(&mut current).unwrap();
            assert_eq!(&current, CpuSet::new().set(vp_index.index()));

            self.partition
                .hcl
                .set_poll_file(
                    self.partition.vp(vp_index).unwrap().cpu_index,
                    control.ring_fd().as_raw_fd(),
                )
                .map_err(Error::Hcl)?;
        }

        UhProcessor::new(driver, &self.partition, self.vp_info, control)
    }

    /// Sets the sidecar remove reason for the processor to be due to a task
    /// running with the given name.
    ///
    /// This is useful for diagnostics.
    pub fn set_sidecar_exit_due_to_task(&self, task: Arc<str>) {
        self.partition
            .vp(self.vp_info.base.vp_index)
            .unwrap()
            .set_sidecar_exit_reason(SidecarExitReason::TaskRequest(task))
    }
}

#[derive(Debug, Inspect)]
struct UhVpInner {
    /// 32 bits per VTL: top bits are VTL 1, bottom bits are VTL 0.
    wake_reasons: AtomicU64,
    #[inspect(skip)]
    waker: RwLock<Option<Waker>>,
    message_queues: VtlArray<MessageQueues, 2>,
    #[inspect(skip)]
    vp_info: TargetVpInfo,
    cpu_index: u32,
    /// Only modified for hardware CVMs. On other types of VMs, since VTL 2
    /// doesn't handle EnableVpVtl, there's no obvious place to set this.
    hcvm_vtl1_enabled: Mutex<bool>,
    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    #[inspect(with = "|arr| inspect::iter_by_index(arr.iter().map(|v| v.lock().is_some()))")]
    hv_start_enable_vtl_vp: VtlArray<Mutex<Option<Box<hvdef::hypercall::InitialVpContextX64>>>, 2>,
    sidecar_exit_reason: Mutex<Option<SidecarExitReason>>,
}

#[cfg_attr(not(guest_arch = "x86_64"), allow(dead_code))]
#[derive(Debug, Inspect)]
struct TlbLockInfo {
    /// The set of VPs that are waiting for this VP to release the TLB lock.
    #[inspect(with = "|bb| inspect::iter_by_index(bb.iter().map(|v| *v))")]
    blocked_vps: BitBox<AtomicU64>,
    /// The set of VPs that are holding the TLB lock and preventing this VP
    /// from proceeding.
    #[inspect(with = "|bb| inspect::iter_by_index(bb.iter().map(|v| *v))")]
    blocking_vps: BitBox<AtomicU64>,
    /// The count of blocking VPs. This should always be equivalent to
    /// `blocking_vps.count_ones()`, however it is accessible in a single
    /// atomic operation while counting is not.
    blocking_vp_count: AtomicU32,
    /// Whether the VP is sleeping due to a TLB lock.
    sleeping: AtomicBool,
}

#[cfg_attr(not(guest_arch = "x86_64"), allow(dead_code))]
impl TlbLockInfo {
    fn new(vp_count: usize) -> Self {
        Self {
            blocked_vps: BitVec::repeat(false, vp_count).into_boxed_bitslice(),
            blocking_vps: BitVec::repeat(false, vp_count).into_boxed_bitslice(),
            blocking_vp_count: AtomicU32::new(0),
            sleeping: false.into(),
        }
    }
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct WakeReason {
    extint: bool,
    message_queues: bool,
    hv_start_enable_vtl_vp: bool,
    intcon: bool,
    update_proxy_irr_filter: bool,
    #[bits(27)]
    _reserved: u32,
}

impl WakeReason {
    // Convenient constants.
    const EXTINT: Self = Self::new().with_extint(true);
    const MESSAGE_QUEUES: Self = Self::new().with_message_queues(true);
    #[cfg(guest_arch = "x86_64")]
    const HV_START_ENABLE_VP_VTL: Self = Self::new().with_hv_start_enable_vtl_vp(true); // StartVp/EnableVpVtl handling
    const INTCON: Self = Self::new().with_intcon(true);
    #[cfg(guest_arch = "x86_64")]
    const UPDATE_PROXY_IRR_FILTER: Self = Self::new().with_update_proxy_irr_filter(true);
}

/// Immutable access to useful bits of Partition state.
impl UhPartition {
    /// Revokes guest VSM.
    pub fn revoke_guest_vsm(&self) -> Result<(), RevokeGuestVsmError> {
        let mut vsm_state = self.inner.guest_vsm.write();

        if matches!(*vsm_state, GuestVsmState::Enabled { vtl1: _ }) {
            return Err(RevokeGuestVsmError::Vtl1AlreadyEnabled);
        }

        *vsm_state = GuestVsmState::NotPlatformSupported;

        if !self.inner.isolation.is_hardware_isolated() {
            self.inner
                .hcl
                .set_guest_vsm_partition_config(false)
                .map_err(RevokeGuestVsmError::SetGuestVsmConfig)?;
        } else {
            let mut cpuid_lock = self.inner.cpuid.lock();
            let current_result =
                cpuid_lock.result(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, 0, &[0; 4]);

            let mut features = hvdef::HvFeatures::from(u128::from_le_bytes(
                current_result
                    .iter()
                    .flat_map(|i| i.to_le_bytes())
                    .collect::<Vec<u8>>()
                    .try_into()
                    .unwrap(),
            ));

            let privileges = hvdef::HvPartitionPrivilege::from(features.privileges());
            features.set_privileges(privileges.with_access_vsm(false).into());

            let split_u128 = |x: u128| -> [u32; 4] {
                let bytes = x.to_le_bytes();
                [
                    u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
                    u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                    u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
                    u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
                ]
            };

            cpuid_lock.update_result(
                hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES,
                0,
                &split_u128(features.into()),
            );
        }

        Ok(())
    }

    /// Returns the current hypervisor reference time, in 100ns units.
    pub fn reference_time(&self) -> u64 {
        self.inner
            .hcl
            .reference_time()
            .expect("should not fail to get the reference time")
    }
}

impl virt::Partition for UhPartition {
    fn supports_reset(&self) -> Option<&dyn virt::ResetPartition<Error = Self::Error>> {
        None
    }

    fn caps(&self) -> &PartitionCapabilities {
        &self.inner.caps
    }

    fn request_msi(&self, vtl: Vtl, request: MsiRequest) {
        self.inner
            .request_msi(vtl.try_into().expect("higher vtl not configured"), request)
    }

    fn request_yield(&self, _vp_index: VpIndex) {
        unimplemented!()
    }
}

impl virt::X86Partition for UhPartition {
    fn ioapic_routing(&self) -> Arc<dyn IoApicRouting> {
        self.inner.clone()
    }

    fn pulse_lint(&self, vp_index: VpIndex, vtl: Vtl, lint: u8) {
        let vtl = GuestVtl::try_from(vtl).expect("higher vtl not configured");
        if let Some(apic) = &self.inner.lapic {
            apic[vtl].lint(vp_index, lint.into(), |vp_index| {
                self.inner
                    .vp(vp_index)
                    .unwrap()
                    .wake(vtl, WakeReason::INTCON);
            });
        } else if lint == 0 {
            self.inner
                .vp(vp_index)
                .unwrap()
                .wake(vtl, WakeReason::EXTINT);
        } else {
            unimplemented!()
        }
    }
}

impl UhPartitionInner {
    fn vp(&self, index: VpIndex) -> Option<&'_ UhVpInner> {
        self.vps.get(index.index() as usize)
    }

    /// For requester VP to issue `proxy_irr_blocked` update to other VPs
    #[cfg(guest_arch = "x86_64")]
    fn request_proxy_irr_filter_update(&self, vtl: GuestVtl, device_vector: u8, req_vp_index: u32) {
        tracing::debug!(
            ?vtl,
            device_vector,
            req_vp_index,
            "request_proxy_irr_filter_update"
        );

        // Add given vector to partition global device vector table (VTL0 only for now)
        {
            let mut device_vector_table = self.device_vector_table.write();
            device_vector_table.set(device_vector as usize, true);
        }

        // Wake all other VPs for their `proxy_irr_blocked` filter update
        for vp in self.vps.iter() {
            if vp.cpu_index != req_vp_index {
                vp.wake(vtl, WakeReason::UPDATE_PROXY_IRR_FILTER);
            }
        }
    }

    /// Get current partition global device irr vectors (VTL0 for now)
    #[cfg(guest_arch = "x86_64")]
    fn fill_device_vectors(&self, _vtl: GuestVtl, irr_vectors: &mut IrrBitmap) {
        let device_vector_table = self.device_vector_table.read();
        for idx in device_vector_table.iter_ones() {
            irr_vectors.set(idx, true);
        }
    }

    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        let mut wake_vps = false;
        resp.field_mut(
            "enter_modes",
            &mut inspect::adhoc_mut(|req| {
                let update = req.is_update();
                {
                    let mut modes = self.enter_modes.lock();
                    modes.inspect_mut(req);
                    if update {
                        self.enter_modes_atomic.store(
                            hcl::protocol::EnterModes::from(*modes).into(),
                            Ordering::Relaxed,
                        );
                        wake_vps = true;
                    }
                }
            }),
        );

        // Wake VPs to propagate updates.
        if wake_vps {
            for vp in self.vps.iter() {
                vp.wake_vtl2();
            }
        }
    }

    // TODO VBS GUEST VSM: enable for aarch64
    #[cfg_attr(guest_arch = "aarch64", allow(dead_code))]
    fn vsm_status(&self) -> Result<HvRegisterVsmPartitionStatus, hcl::ioctl::Error> {
        // TODO: It might be possible to cache VsmPartitionStatus.
        let reg = self.hcl.get_vp_register(
            HvAllArchRegisterName::VsmPartitionStatus,
            HvInputVtl::CURRENT_VTL,
        )?;
        Ok(reg.as_u64().into())
    }
}

impl virt::Synic for UhPartition {
    fn post_message(&self, vtl: Vtl, vp_index: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        let vtl = GuestVtl::try_from(vtl).expect("higher vtl not configured");
        let Some(vp) = self.inner.vp(vp_index) else {
            tracelimit::warn_ratelimited!(
                vp = vp_index.index(),
                "invalid vp target for post_message"
            );
            return;
        };

        vp.post_message(
            vtl,
            sint,
            &hvdef::HvMessage::new(hvdef::HvMessageType(typ), 0, payload),
        );
    }

    fn new_guest_event_port(&self) -> Box<dyn vmcore::synic::GuestEventPort> {
        Box::new(UhEventPort {
            partition: Arc::downgrade(&self.inner),
            params: Default::default(),
        })
    }

    fn prefer_os_events(&self) -> bool {
        false
    }

    fn monitor_support(&self) -> Option<&dyn virt::SynicMonitor> {
        // TODO TDX TODO SNP: Disable monitor support for TDX and SNP as support
        // for VTL2 protections is needed to emulate this page, which is not
        // implemented yet.
        if self.inner.isolation.is_hardware_isolated() {
            None
        } else {
            Some(self)
        }
    }
}

impl virt::SynicMonitor for UhPartition {
    fn set_monitor_page(&self, gpa: Option<u64>) -> anyhow::Result<()> {
        let old_gpa = self.inner.monitor_page.set_gpa(gpa);
        if let Some(old_gpa) = old_gpa {
            self.inner
                .hcl
                .modify_vtl_protection_mask(
                    MemoryRange::new(old_gpa..old_gpa + HV_PAGE_SIZE),
                    hvdef::HV_MAP_GPA_PERMISSIONS_ALL,
                    HvInputVtl::CURRENT_VTL,
                )
                .context("failed to unregister old monitor page")?;

            tracing::debug!(old_gpa, "unregistered monitor page");
        }

        if let Some(gpa) = gpa {
            // Disallow VTL0 from writing to the page, so we'll get an intercept. Note that read
            // permissions must be enabled or this doesn't work correctly.
            let result = self
                .inner
                .hcl
                .modify_vtl_protection_mask(
                    MemoryRange::new(gpa..gpa + HV_PAGE_SIZE),
                    HvMapGpaFlags::new().with_readable(true),
                    HvInputVtl::CURRENT_VTL,
                )
                .context("failed to register monitor page");

            if result.is_err() {
                // Unset the page so trying to remove it later won't fail too.
                self.inner.monitor_page.set_gpa(None);
                return result;
            }

            tracing::debug!(gpa, "registered monitor page");
        }

        Ok(())
    }

    fn register_monitor(
        &self,
        monitor_id: vmcore::monitor::MonitorId,
        connection_id: u32,
    ) -> Box<dyn Send> {
        self.inner
            .monitor_page
            .register_monitor(monitor_id, connection_id)
    }
}

impl UhPartitionInner {
    #[cfg(guest_arch = "x86_64")]
    pub(crate) fn synic_interrupt(
        &self,
        vp_index: VpIndex,
        vtl: GuestVtl,
    ) -> impl '_ + hv1_emulator::RequestInterrupt {
        move |vector, auto_eoi| {
            self.lapic.as_ref().unwrap()[vtl].synic_interrupt(
                vp_index,
                vector as u8,
                auto_eoi,
                |vp_index| self.vp(vp_index).unwrap().wake(vtl, WakeReason::INTCON),
            );
        }
    }

    #[cfg(guest_arch = "aarch64")]
    fn synic_interrupt(
        &self,
        _vp_index: VpIndex,
        _vtl: GuestVtl,
    ) -> impl '_ + hv1_emulator::RequestInterrupt {
        move |_, _| {}
    }
}

#[derive(Debug)]
struct UhEventPort {
    partition: Weak<UhPartitionInner>,
    params: Arc<Mutex<Option<UhEventPortParams>>>,
}

#[derive(Debug, Copy, Clone)]
struct UhEventPortParams {
    vp: VpIndex,
    sint: u8,
    flag: u16,
    vtl: GuestVtl,
}

impl vmcore::synic::GuestEventPort for UhEventPort {
    fn interrupt(&self) -> vmcore::interrupt::Interrupt {
        let partition = self.partition.clone();
        let params = self.params.clone();
        vmcore::interrupt::Interrupt::from_fn(move || {
            let Some(UhEventPortParams {
                vp,
                sint,
                flag,
                vtl,
            }) = *params.lock()
            else {
                return;
            };
            let Some(partition) = partition.upgrade() else {
                return;
            };
            tracing::trace!(vp = vp.index(), sint, flag, "signal_event");
            if let Some(hv) = partition.hv.as_ref() {
                match hv.synic[vtl].signal_event(
                    &partition.gm[vtl],
                    vp,
                    sint,
                    flag,
                    &mut partition.synic_interrupt(vp, vtl),
                ) {
                    Ok(_) => {}
                    Err(SintProxied) => {
                        tracing::trace!(
                            vp = vp.index(),
                            sint,
                            flag,
                            "forwarding event to untrusted synic"
                        );
                        if let Some(synic) = &partition.untrusted_synic {
                            synic
                                .signal_event(
                                    &partition.gm[vtl],
                                    vp,
                                    sint,
                                    flag,
                                    &mut partition.synic_interrupt(vp, vtl),
                                )
                                .ok();
                        } else {
                            partition.hcl.signal_event_direct(vp.index(), sint, flag)
                        }
                    }
                }
            } else {
                partition.hcl.signal_event_direct(vp.index(), sint, flag);
            }
        })
    }

    fn clear(&mut self) {
        *self.params.lock() = None;
    }

    fn set(
        &mut self,
        vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Result<(), vmcore::synic::HypervisorError> {
        let vtl = GuestVtl::try_from(vtl).expect("higher vtl not configured");
        *self.params.lock() = Some(UhEventPortParams {
            vp: VpIndex::new(vp),
            sint,
            flag,
            vtl,
        });

        Ok(())
    }
}

impl virt::Hv1 for UhPartition {
    type Error = Error;
    type Device = virt::x86::apic_software_device::ApicSoftwareDevice;

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        self.inner.software_devices.is_some().then_some(self)
    }
}

impl virt::DeviceBuilder for UhPartition {
    fn build(&self, vtl: Vtl, device_id: u64) -> Result<Self::Device, Self::Error> {
        let vtl = GuestVtl::try_from(vtl).expect("higher vtl not configured");
        let device = self
            .inner
            .software_devices
            .as_ref()
            .expect("checked in new_virtual_device")
            .new_device(self.interrupt_targets[vtl].clone(), device_id)
            .map_err(Error::NewDevice)?;

        Ok(device)
    }
}

impl virt::VtlMemoryProtection for UhPartition {
    /// TODO CVM GUEST_VSM:
    ///     GH954: Review alternatives to dynamically allocating from VTL2 RAM
    ///     (e.g. reserve pages for this purpose), or constrain it for more
    ///     safety.  The concern is freeing a page but forgetting to reset
    ///     permissions. See PagesAccessibleToLowerVtl for a sample wrapper.
    fn modify_vtl_page_setting(&self, pfn: u64, flags: HvMapGpaFlags) -> anyhow::Result<()> {
        let address = pfn << hvdef::HV_PAGE_SHIFT;
        self.inner
            .hcl
            .modify_vtl_protection_mask(
                MemoryRange::new(address..address + HV_PAGE_SIZE),
                flags,
                HvInputVtl::CURRENT_VTL,
            )
            .context("failed to modify VTL page permissions")
    }
}

struct UhInterruptTarget {
    partition: Arc<UhPartitionInner>,
    vtl: GuestVtl,
}

impl pci_core::msi::MsiInterruptTarget for UhInterruptTarget {
    fn new_interrupt(&self) -> Box<dyn pci_core::msi::MsiControl> {
        let partition = self.partition.clone();
        let vtl = self.vtl;
        Box::new(move |address, data| partition.request_msi(vtl, MsiRequest { address, data }))
    }
}

impl UhPartitionInner {
    fn request_msi(&self, vtl: GuestVtl, request: MsiRequest) {
        if let Some(lapic) = &self.lapic {
            tracing::trace!(?request, "interrupt");
            lapic[vtl].request_interrupt(request.address, request.data, |vp_index| {
                self.vp(vp_index).unwrap().wake(vtl, WakeReason::INTCON)
            });
        } else {
            let (address, data) = request.as_x86();
            if let Err(err) = self.hcl.request_interrupt(
                request.hv_x86_interrupt_control(),
                address.virt_destination().into(),
                data.vector().into(),
                vtl,
            ) {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    address = request.address,
                    data = request.data,
                    "failed to request msi"
                );
            }
        }
    }
}

impl IoApicRouting for UhPartitionInner {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        self.irq_routes.set_irq_route(irq, request)
    }

    // The IO-APIC is always hooked up to VTL0.
    fn assert_irq(&self, irq: u8) {
        self.irq_routes
            .assert_irq(irq, |request| self.request_msi(GuestVtl::Vtl0, request))
    }
}

/// Configure the [`hvdef::HvRegisterVsmPartitionConfig`] register with the
/// values used by underhill.
fn set_vtl2_vsm_partition_config(hcl: &mut Hcl) -> Result<(), Error> {
    // Read available capabilities to determine what to enable.
    let caps = hcl.get_vsm_capabilities().map_err(Error::Hcl)?;
    let hardware_isolated = hcl.isolation().is_hardware_isolated();
    let isolated = hcl.isolation().is_isolated();

    let config = HvRegisterVsmPartitionConfig::new()
        .with_default_vtl_protection_mask(0xF)
        .with_enable_vtl_protection(!hardware_isolated)
        .with_zero_memory_on_reset(!hardware_isolated)
        .with_intercept_cpuid_unimplemented(!hardware_isolated)
        .with_intercept_page(true)
        .with_intercept_unrecoverable_exception(true)
        .with_intercept_not_present(caps.intercept_not_present_available() && !isolated)
        .with_intercept_acceptance(isolated)
        .with_intercept_enable_vtl_protection(isolated && !hardware_isolated)
        .with_intercept_system_reset(caps.intercept_system_reset_available());

    hcl.set_vtl2_vsm_partition_config(config)
        .map_err(Error::VsmPartitionConfig)
}

/// Configuration parameters supplied to [`UhProtoPartition::new`].
///
/// These do not include runtime resources.
pub struct UhPartitionNewParams<'a> {
    /// The isolation type for the partition.
    pub isolation: IsolationType,
    /// Hide isolation from the guest. The guest will run as if it is not
    /// isolated.
    pub hide_isolation: bool,
    /// The memory layout for lower VTLs.
    pub lower_vtl_memory_layout: &'a MemoryLayout,
    /// The guest processor topology.
    pub topology: &'a ProcessorTopology,
    /// The unparsed CVM cpuid info.
    // TODO: move parsing up a layer.
    pub cvm_cpuid_info: Option<&'a [u8]>,
    /// The unparsed CVM secrets page.
    pub snp_secrets: Option<&'a [u8]>,
    /// Whether underhill was configured to support guest vsm for CVMs
    pub env_cvm_guest_vsm: bool,
    /// The virtual top of memory for hardware-isolated VMs.
    ///
    /// Must be a power of two.
    pub vtom: Option<u64>,
    /// Handle synic messages and events.
    ///
    /// On TDX, this prevents the hypervisor from getting vmtdcall exits.
    pub handle_synic: bool,
    /// Do not hotplug sidecar VPs on their first exit. Just continue running
    /// the VP remotely.
    pub no_sidecar_hotplug: bool,
    /// Use MMIO access hypercalls.
    pub use_mmio_hypercalls: bool,
    /// Intercept guest debug exceptions to support gdbstub.
    pub intercept_debug_exceptions: bool,
}

/// Parameters to [`UhProtoPartition::build`].
pub struct UhLateParams<'a> {
    /// Guest memory for untrusted devices, like overlay pages.
    pub shared_memory: Option<GuestMemory>,
    /// Guest memory for lower VTLs.
    pub gm: VtlArray<GuestMemory, 2>,
    /// The CPUID leaves to expose to the guest.
    #[cfg(guest_arch = "x86_64")]
    pub cpuid: Vec<CpuidLeaf>,
    /// The mesh sender to use for crash notifications.
    // FUTURE: remove mesh dependency from this layer.
    pub crash_notification_send: mesh::Sender<VtlCrash>,
    /// The VM time source.
    pub vmtime: &'a VmTimeSource,
    /// An object to call to change host visibility on guest memory.
    pub isolated_memory_protector: Option<Arc<dyn ProtectIsolatedMemory>>,
    /// Allocator for shared visibility pages.
    pub shared_vis_pages_pool: Option<page_pool_alloc::PagePoolAllocator>,
    /// Allocator for private visibility pages.
    pub private_vis_pages_pool: Option<page_pool_alloc::PagePoolAllocator>,
}

/// Trait for CVM-related protections on guest memory.
pub trait ProtectIsolatedMemory: Send + Sync {
    /// Changes host visibility on guest memory.
    fn change_host_visibility(&self, shared: bool, gpns: &[u64]) -> Result<(), (HvError, usize)>;

    /// Queries host visibility on guest memory.
    fn query_host_visibility(
        &self,
        gpns: &[u64],
        host_visibility: &mut [HostVisibilityType],
    ) -> Result<(), (HvError, usize)>;

    /// Gets the default protections/permissions for VTL 0.
    fn default_vtl0_protections(&self) -> HvMapGpaFlags;

    /// Changes the default protections/permissions for a VTL. For VBS-isolated
    /// VMs, the protections apply to all vtls lower than the specified one. For
    /// hardware-isolated VMs, they apply just to the given vtl.
    fn change_default_vtl_protections(
        &self,
        vtl: GuestVtl,
        protections: HvMapGpaFlags,
    ) -> Result<(), HvError>;

    /// Changes the vtl protections on a range of guest memory.
    fn change_vtl_protections(
        &self,
        vtl: GuestVtl,
        gpns: &[u64],
        protections: HvMapGpaFlags,
    ) -> Result<(), (HvError, usize)>;

    /// Retrieves a protector for the hypercall code page overlay for a target
    /// VTL.
    fn hypercall_overlay_protector(
        self: Arc<Self>,
        vtl: GuestVtl,
    ) -> Box<dyn VtlProtectHypercallOverlay>;

    /// Changes the overlay for the hypercall code page for a target VTL.
    fn change_hypercall_overlay(&self, vtl: GuestVtl, gpn: u64);

    /// Disables the overlay for the hypercall code page for a target VTL.
    fn disable_hypercall_overlay(&self, vtl: GuestVtl);

    /// Alerts the memory protector that vtl 1 is ready to set vtl protections
    /// on lower-vtl memory, and that these protections should be enforced.
    fn set_vtl1_protections_enabled(&self);

    /// Whether VTL 1 is prepared to modify vtl protections on lower-vtl memory,
    /// and therefore whether these protections should be enforced.
    fn vtl1_protections_enabled(&self) -> bool;
}

/// A partially built partition. Used to allow querying partition capabilities
/// before fully instantiating the partition.
pub struct UhProtoPartition<'a> {
    params: UhPartitionNewParams<'a>,
    hcl: Hcl,
    #[cfg(guest_arch = "x86_64")]
    cvm_cpuid: Option<cvm_cpuid::CpuidResults>,
    guest_vsm_available: bool,
}

impl<'a> UhProtoPartition<'a> {
    /// Creates a new prototype partition.
    ///
    /// `driver(cpu)` returns the driver to use for polling the sidecar device
    /// whose base CPU is `cpu`.
    pub fn new<T: SpawnDriver>(
        params: UhPartitionNewParams<'a>,
        driver: impl FnMut(u32) -> T,
    ) -> Result<Self, Error> {
        let hcl_isolation = match params.isolation {
            IsolationType::None => hcl::ioctl::IsolationType::None,
            IsolationType::Vbs => hcl::ioctl::IsolationType::Vbs,
            IsolationType::Snp => hcl::ioctl::IsolationType::Snp,
            IsolationType::Tdx => hcl::ioctl::IsolationType::Tdx,
        };

        // Try to open the sidecar device, if it is present.
        let sidecar = sidecar_client::SidecarClient::new(driver).map_err(Error::Sidecar)?;

        let hcl = Hcl::new(hcl_isolation, sidecar).map_err(Error::Hcl)?;

        // Set the hypercalls that this process will use.
        let mut allowed_hypercalls = vec![
            hvdef::HypercallCode::HvCallGetVpRegisters,
            hvdef::HypercallCode::HvCallSetVpRegisters,
            hvdef::HypercallCode::HvCallInstallIntercept,
            hvdef::HypercallCode::HvCallTranslateVirtualAddress,
            hvdef::HypercallCode::HvCallPostMessageDirect,
            hvdef::HypercallCode::HvCallSignalEventDirect,
            hvdef::HypercallCode::HvCallModifyVtlProtectionMask,
            hvdef::HypercallCode::HvCallTranslateVirtualAddressEx,
            hvdef::HypercallCode::HvCallCheckSparseGpaPageVtlAccess,
            hvdef::HypercallCode::HvCallAssertVirtualInterrupt,
            hvdef::HypercallCode::HvCallGetVpIndexFromApicId,
            hvdef::HypercallCode::HvCallAcceptGpaPages,
            hvdef::HypercallCode::HvCallModifySparseGpaPageHostVisibility,
        ];

        if params.isolation.is_hardware_isolated() {
            allowed_hypercalls.extend(vec![
                hvdef::HypercallCode::HvCallEnablePartitionVtl,
                hvdef::HypercallCode::HvCallRetargetDeviceInterrupt,
                hvdef::HypercallCode::HvCallEnableVpVtl,
            ]);
        }

        if params.use_mmio_hypercalls {
            allowed_hypercalls.extend(vec![
                hvdef::HypercallCode::HvCallMemoryMappedIoRead,
                hvdef::HypercallCode::HvCallMemoryMappedIoWrite,
            ]);
        }

        hcl.set_allowed_hypercalls(allowed_hypercalls.as_slice());

        #[cfg(guest_arch = "x86_64")]
        let cvm_cpuid = match params.isolation {
            IsolationType::Snp => Some(
                cvm_cpuid::CpuidResults::new(cvm_cpuid::CpuidResultsIsolationType::Snp {
                    cpuid_pages: params.cvm_cpuid_info.unwrap(),
                })
                .map_err(Error::CvmCpuid)?,
            ),
            IsolationType::Tdx => Some(
                cvm_cpuid::CpuidResults::new(cvm_cpuid::CpuidResultsIsolationType::Tdx)
                    .map_err(Error::CvmCpuid)?,
            ),
            IsolationType::Vbs | IsolationType::None => None,
        };

        let guest_vsm_available = Self::check_guest_vsm_support(
            &hcl,
            &params,
            #[cfg(guest_arch = "x86_64")]
            cvm_cpuid.as_ref(),
        )?;

        Ok(UhProtoPartition {
            hcl,
            params,
            #[cfg(guest_arch = "x86_64")]
            cvm_cpuid,
            guest_vsm_available,
        })
    }

    /// Returns whether VSM support will be available to the guest.
    pub fn guest_vsm_available(&self) -> bool {
        self.guest_vsm_available
    }

    /// Returns a new Underhill partition.
    pub async fn build(
        self,
        late_params: UhLateParams<'_>,
    ) -> Result<(UhPartition, Vec<UhProcessorBox>), Error> {
        let Self {
            mut hcl,
            params,
            #[cfg(guest_arch = "x86_64")]
            cvm_cpuid,
            guest_vsm_available,
        } = self;
        let isolation = params.isolation;
        let is_hardware_isolated = isolation.is_hardware_isolated();

        // Intercept Debug Exceptions
        // On TDX because all OpenHCL TDs today have the debug policy bit set,
        // OpenHCL registers for the intercepts itself.
        // However, on non-TDX platforms hypervisor installs the
        // intercept on behalf of the guest.
        if params.intercept_debug_exceptions {
            if !cfg!(feature = "gdb") {
                return Err(Error::InvalidDebugConfiguration);
            }

            cfg_if::cfg_if! {
                if #[cfg(guest_arch = "x86_64")] {
                    if isolation != IsolationType::Tdx {
                        let debug_exception_vector = 0x1;
                        hcl.register_intercept(
                            HvInterceptType::HvInterceptTypeException,
                            HV_INTERCEPT_ACCESS_MASK_EXECUTE,
                            HvInterceptParameters::new_exception(debug_exception_vector),
                        )
                        .map_err(|err| Error::InstallIntercept(HvInterceptType::HvInterceptTypeException, err))?;
                    }
                } else {
                    return Err(Error::InvalidDebugConfiguration);
                }
            }
        }

        if !is_hardware_isolated {
            if cfg!(guest_arch = "x86_64") {
                hcl.register_intercept(
                    HvInterceptType::HvInterceptTypeX64Msr,
                    HV_INTERCEPT_ACCESS_MASK_READ_WRITE,
                    HvInterceptParameters::new_zeroed(),
                )
                .map_err(|err| {
                    Error::InstallIntercept(HvInterceptType::HvInterceptTypeX64Msr, err)
                })?;

                hcl.register_intercept(
                    HvInterceptType::HvInterceptTypeX64ApicEoi,
                    HV_INTERCEPT_ACCESS_MASK_WRITE,
                    HvInterceptParameters::new_zeroed(),
                )
                .map_err(|err| {
                    Error::InstallIntercept(HvInterceptType::HvInterceptTypeX64ApicEoi, err)
                })?;
            } else {
                if false {
                    todo!("AARCH64_TODO");
                }
            }
        }

        if isolation == IsolationType::Snp {
            // SNP VMs register for the #VC exception to support reflect-VC.
            hcl.register_intercept(
                HvInterceptType::HvInterceptTypeException,
                HV_INTERCEPT_ACCESS_MASK_EXECUTE,
                HvInterceptParameters::new_exception(0x1D),
            )
            .map_err(|err| {
                Error::InstallIntercept(HvInterceptType::HvInterceptTypeException, err)
            })?;

            // Get the register tweak bitmap from secrets page.
            let mut bitmap = [0u8; 64];
            if let Some(secrets) = params.snp_secrets {
                bitmap.copy_from_slice(
                    &secrets
                        [REG_TWEAK_BITMAP_OFFSET..REG_TWEAK_BITMAP_OFFSET + REG_TWEAK_BITMAP_SIZE],
                );
            }
            hcl.set_snp_register_bitmap(bitmap);
        }

        set_vtl2_vsm_partition_config(&mut hcl)?;

        // Do per-VP HCL initialization.
        hcl.add_vps(params.topology.vp_count())
            .map_err(Error::Hcl)?;

        let vps: Vec<_> = params
            .topology
            .vps_arch()
            .map(|vp_info| {
                // TODO: determine CPU index, which in theory could be different
                // from the VP index.
                let cpu_index = vp_info.base.vp_index.index();
                UhVpInner::new(cpu_index, vp_info)
            })
            .collect();

        // Enable support for VPCI devices if the hypervisor supports it.
        #[cfg(guest_arch = "x86_64")]
        let software_devices = {
            let res = if !is_hardware_isolated {
                hcl.register_intercept(
                    HvInterceptType::HvInterceptTypeRetargetInterruptWithUnknownDeviceId,
                    HV_INTERCEPT_ACCESS_MASK_EXECUTE,
                    HvInterceptParameters::new_zeroed(),
                )
            } else {
                Ok(())
            };
            match res {
                Ok(()) => Some(ApicSoftwareDevices::new(
                    params.topology.vps_arch().map(|vp| vp.apic_id).collect(),
                )),
                Err(HvError::InvalidParameter | HvError::AccessDenied) => None,
                Err(err) => {
                    return Err(Error::InstallIntercept(
                        HvInterceptType::HvInterceptTypeRetargetInterruptWithUnknownDeviceId,
                        err,
                    ));
                }
            }
        };

        #[cfg(guest_arch = "aarch64")]
        let software_devices = None;

        #[cfg(guest_arch = "aarch64")]
        let (lapic, caps, cpuid) = (
            None,
            virt::aarch64::Aarch64PartitionCapabilities {},
            Mutex::new(CpuidLeafSet::new(Vec::new())),
        );

        let vsm_state = if guest_vsm_available {
            if is_hardware_isolated {
                tracing::warn!("Advertising guest vsm as being supported to the guest. This feature is in development, so the guest might crash.");
            }
            GuestVsmState::NotGuestEnabled
        } else {
            GuestVsmState::NotPlatformSupported
        };

        #[cfg(guest_arch = "x86_64")]
        let cpuid = UhPartition::construct_cpuid_results(
            &late_params.cpuid,
            params.topology,
            // Note: currently, guest_vsm_available can only set to true for
            // hardware-isolated VMs. There aren't any other scenarios at the
            // moment that will require underhill to expose vsm support through
            // the cpuid results.
            guest_vsm_available,
            params.vtom,
            isolation,
            params.hide_isolation,
        );

        #[cfg(guest_arch = "x86_64")]
        let caps = UhPartition::construct_capabilities(
            params.topology,
            &cpuid,
            cvm_cpuid.as_ref(),
            isolation,
            params.hide_isolation,
        );

        #[cfg(guest_arch = "x86_64")]
        let cpuid = Mutex::new(cpuid);

        #[cfg(guest_arch = "x86_64")]
        let lapic = is_hardware_isolated.then(|| {
            VtlArray::from_fn(|_| {
                LocalApicSet::builder()
                    .x2apic_capable(caps.x2apic)
                    .hyperv_enlightenments(true)
                    .build()
            })
        });

        #[cfg(guest_arch = "aarch64")]
        let hv = None;

        // If we're emulating the APIC, then we also must emulate the hypervisor
        // enlightenments, since the hypervisor can't support enlightenments
        // without also providing an APIC.
        //
        // Additionally, TDX provides hardware APIC emulation but we still need
        // to emulate the hypervisor enlightenments.
        #[cfg(guest_arch = "x86_64")]
        let hv = if lapic.is_some() {
            let tsc_frequency = get_tsc_frequency(isolation)?;

            let ref_time = Box::new(TscReferenceTimeSource::new(tsc_frequency));
            Some(GlobalHv::new(hv1_emulator::hv::GlobalHvParams {
                max_vp_count: params.topology.vp_count(),
                vendor: caps.vendor,
                tsc_frequency,
                ref_time,
                hypercall_page_protectors: VtlArray::from_fn(|vtl| {
                    late_params.isolated_memory_protector.as_ref().map(|p| {
                        p.clone()
                            .hypercall_overlay_protector(vtl.try_into().expect("no vtl 2"))
                    })
                }),
            }))
        } else {
            None
        };

        let untrusted_synic = if params.handle_synic {
            if matches!(isolation, IsolationType::Tdx) {
                // Create a second synic to fully manage the untrusted SINTs
                // here. At time of writing, the hypervisor does not support
                // sharing the untrusted SINTs with the TDX L1. Even if it did,
                // performance would be poor for cases where the L1 implements
                // high-performance devices.
                if !params.hide_isolation {
                    Some(GlobalSynic::new(params.topology.vp_count()))
                } else {
                    None
                }
            } else {
                // The hypervisor will manage the untrusted SINTs (or the whole
                // synic for non-hardware-isolated VMs), but some event ports
                // and message ports are implemented here. Register an intercept
                // to handle HvSignalEvent and HvPostMessage hypercalls when the
                // hypervisor doesn't recognize the connection ID.
                hcl.register_intercept(
                    HvInterceptType::HvInterceptTypeUnknownSynicConnection,
                    HV_INTERCEPT_ACCESS_MASK_EXECUTE,
                    HvInterceptParameters::new_zeroed(),
                )
                .expect("registering synic intercept cannot fail");
                None
            }
        } else {
            None
        };

        #[cfg(guest_arch = "x86_64")]
        let cvm_state = cvm_cpuid
            .map(|cpuid| Self::construct_cvm_state(&params, &late_params, cpuid))
            .transpose()?;
        #[cfg(guest_arch = "aarch64")]
        let cvm_state = None;

        let enter_modes = EnterModes::default();

        let partition = Arc::new(UhPartitionInner {
            hcl,
            vps,
            irq_routes: Default::default(),
            caps,
            enter_modes: Mutex::new(enter_modes),
            enter_modes_atomic: u8::from(hcl::protocol::EnterModes::from(enter_modes)).into(),
            gm: late_params.gm,
            shared_memory: late_params.shared_memory,
            cpuid,
            crash_notification_send: late_params.crash_notification_send,
            monitor_page: MonitorPage::new(),
            software_devices,
            lower_vtl_memory_layout: params.lower_vtl_memory_layout.clone(),
            lapic,
            vmtime: late_params.vmtime.clone(),
            isolation,
            hide_isolation: params.hide_isolation,
            hv,
            untrusted_synic,
            guest_vsm: RwLock::new(vsm_state),
            isolated_memory_protector: late_params.isolated_memory_protector.clone(),
            shared_vis_pages_pool: late_params.shared_vis_pages_pool,
            private_vis_pages_pool: late_params.private_vis_pages_pool,
            no_sidecar_hotplug: params.no_sidecar_hotplug.into(),
            use_mmio_hypercalls: params.use_mmio_hypercalls,
            backing_shared: BackingShared::new(
                isolation,
                BackingSharedParams {
                    cvm_state,
                    vp_count: params.topology.vp_count(),
                },
            )?,
            #[cfg(guest_arch = "x86_64")]
            device_vector_table: RwLock::new(IrrBitmap::new(Default::default())),
            intercept_debug_exceptions: params.intercept_debug_exceptions,
        });

        if cfg!(guest_arch = "x86_64") {
            // Intercept all IOs unless opted out.
            partition.manage_io_port_intercept_region(0, !0, true);
        }

        let vps = params
            .topology
            .vps_arch()
            .map(|vp_info| UhProcessorBox {
                partition: partition.clone(),
                vp_info,
            })
            .collect();

        Ok((
            UhPartition {
                inner: partition.clone(),
                interrupt_targets: VtlArray::from_fn(|vtl| {
                    Arc::new(UhInterruptTarget {
                        partition: partition.clone(),
                        vtl: vtl.try_into().unwrap(),
                    })
                }),
            },
            vps,
        ))
    }
}

impl UhPartition {
    /// Gets the guest OS ID for VTL0.
    pub fn vtl0_guest_os_id(&self) -> Result<HvGuestOsId, Error> {
        // If Underhill is emulating the hypervisor interfaces, get this value
        // from the emulator. This happens when running under hardware isolation
        // or when configured for testing.
        let id = if let Some(hv) = self.inner.hv.as_ref() {
            hv.guest_os_id(Vtl::Vtl0)
        } else {
            // Ask the hypervisor for this value.
            let reg_value = self
                .inner
                .hcl
                .get_vp_register(HvAllArchRegisterName::GuestOsId, Vtl::Vtl0.into())
                .map_err(Error::Hcl)?;

            HvGuestOsId::from(reg_value.as_u64())
        };
        Ok(id)
    }

    /// Configures guest accesses to IO ports in `range` to go directly to the
    /// host.
    ///
    /// When the return value is dropped, the ports will be unregistered.
    pub fn register_host_io_port_fast_path(
        &self,
        range: RangeInclusive<u16>,
    ) -> HostIoPortFastPathHandle {
        // There is no way to provide a fast path for some hardware isolated
        // VM architectures. The devices that do use this facility are not
        // enabled on hardware isolated VMs.
        assert!(!self.inner.isolation.is_hardware_isolated());

        self.inner
            .manage_io_port_intercept_region(*range.start(), *range.end(), false);
        HostIoPortFastPathHandle {
            inner: Arc::downgrade(&self.inner),
            begin: *range.start(),
            end: *range.end(),
        }
    }

    /// Enables or disables the PM timer assist.
    pub fn set_pm_timer_assist(&self, port: Option<u16>) -> Result<(), HvError> {
        self.inner.hcl.set_pm_timer_assist(port)
    }
}

impl UhProtoPartition<'_> {
    /// Whether Guest VSM is available to the guest. If so, for hardware CVMs,
    /// it is safe to expose Guest VSM support via cpuid.
    fn check_guest_vsm_support(
        hcl: &Hcl,
        params: &UhPartitionNewParams<'_>,
        #[cfg(guest_arch = "x86_64")] cvm_cpuid: Option<&cvm_cpuid::CpuidResults>,
    ) -> Result<bool, Error> {
        match params.isolation {
            IsolationType::None | IsolationType::Vbs => {}
            #[cfg(guest_arch = "x86_64")]
            IsolationType::Tdx => return Ok(false), // TODO TDX GUEST_VSM
            #[cfg(guest_arch = "x86_64")]
            IsolationType::Snp => {
                if !params.env_cvm_guest_vsm {
                    return Ok(false);
                }
                // Require RMP Query
                let rmp_query = x86defs::cpuid::ExtendedSevFeaturesEax::from(
                    cvm_cpuid
                        .unwrap()
                        .registered_result(x86defs::cpuid::CpuidFunction::ExtendedSevFeatures, 0)
                        .eax,
                )
                .rmp_query();

                if !rmp_query {
                    tracing::info!("rmp query not supported, cannot enable vsm");
                    return Ok(false);
                }
            }
            #[allow(unreachable_patterns)]
            isolation => panic!("unsupported isolation type {:?}", isolation),
        }

        #[cfg(guest_arch = "x86_64")]
        let privs = {
            let result = safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, 0);
            result.eax as u64 | ((result.ebx as u64) << 32)
        };

        #[cfg(guest_arch = "aarch64")]
        let privs = hcl
            .get_vp_register(
                HvArm64RegisterName::PrivilegesAndFeaturesInfo,
                HvInputVtl::CURRENT_VTL,
            )
            .map_err(Error::Hcl)?
            .as_u64();

        if !hvdef::HvPartitionPrivilege::from(privs).access_vsm() {
            return Ok(false);
        }
        let guest_vsm_config = hcl.get_guest_vsm_partition_config().map_err(Error::Hcl)?;
        Ok(guest_vsm_config.maximum_vtl() >= u8::from(GuestVtl::Vtl1))
    }

    #[cfg(guest_arch = "x86_64")]
    /// Constructs partition-wide CVM state.
    fn construct_cvm_state(
        params: &UhPartitionNewParams<'_>,
        late_params: &UhLateParams<'_>,
        cpuid: cvm_cpuid::CpuidResults,
    ) -> Result<UhCvmPartitionState, Error> {
        let vp_count = params.topology.vp_count() as usize;
        let tlb_lock_info = (0..vp_count)
            .map(|_| VtlArray::from_fn(|_| TlbLockInfo::new(vp_count)))
            .collect();
        let tlb_locked_vps =
            VtlArray::from_fn(|_| BitVec::repeat(false, vp_count).into_boxed_bitslice());
        Ok(UhCvmPartitionState {
            cpuid,
            tlb_locked_vps,
            tlb_lock_info,
            shared_memory: late_params
                .shared_memory
                .clone()
                .ok_or(Error::MissingSharedMemory)?,
        })
    }
}

impl UhPartition {
    #[cfg(guest_arch = "x86_64")]
    /// Constructs the set of cpuid results to show to the guest
    fn construct_cpuid_results(
        initial_cpuid: &[CpuidLeaf],
        topology: &ProcessorTopology<vm_topology::processor::x86::X86Topology>,
        access_vsm: bool,
        vtom: Option<u64>,
        isolation: IsolationType,
        hide_isolation: bool,
    ) -> CpuidLeafSet {
        let mut cpuid = CpuidLeafSet::new(Vec::new());

        if isolation.is_hardware_isolated() {
            // Get the hypervisor version from the host. This is just for
            // reporting purposes, so it is safe even if the hypervisor is not
            // trusted.
            let hv_version = safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_VERSION, 0);
            cpuid.extend(&hv1_emulator::cpuid::hv_cpuid_leaves(
                topology,
                if hide_isolation {
                    IsolationType::None
                } else {
                    isolation
                },
                access_vsm,
                [
                    hv_version.eax,
                    hv_version.ebx,
                    hv_version.ecx,
                    hv_version.edx,
                ],
                vtom,
            ));
        }
        cpuid.extend(initial_cpuid);

        cpuid
    }

    #[cfg(guest_arch = "x86_64")]
    /// Computes the partition capabilities
    fn construct_capabilities(
        topology: &ProcessorTopology,
        cpuid: &CpuidLeafSet,
        cvm_cpuid: Option<&cvm_cpuid::CpuidResults>,
        isolation: IsolationType,
        hide_isolation: bool,
    ) -> virt::x86::X86PartitionCapabilities {
        let mut native_cpuid_fn;
        let mut cvm_cpuid_fn;

        // Determine the method to get cpuid results for the guest when
        // computing partition capabilities.
        let cpuid_fn: &mut dyn FnMut(u32, u32) -> [u32; 4] = if let Some(cvm_cpuid) = cvm_cpuid {
            // Use the filtered CPUID to determine capabilities.
            let bsp = topology.vp_arch(VpIndex::BSP).apic_id;
            cvm_cpuid_fn = move |leaf, sub_leaf| {
                let CpuidResult { eax, ebx, ecx, edx } = cvm_cpuid.guest_result(
                    x86defs::cpuid::CpuidFunction(leaf),
                    sub_leaf,
                    &cvm_cpuid::CpuidGuestState {
                        xfem: 1,
                        xss: 0,
                        cr4: 0,
                        apic_id: bsp,
                    },
                );
                cpuid.result(leaf, sub_leaf, &[eax, ebx, ecx, edx])
            };
            &mut cvm_cpuid_fn
        } else {
            // Just use the native cpuid.
            native_cpuid_fn = |leaf, sub_leaf| {
                let CpuidResult { eax, ebx, ecx, edx } = safe_intrinsics::cpuid(leaf, sub_leaf);
                cpuid.result(leaf, sub_leaf, &[eax, ebx, ecx, edx])
            };
            &mut native_cpuid_fn
        };

        // Compute and validate capabilities.
        let mut caps = virt::x86::X86PartitionCapabilities::from_cpuid(topology, cpuid_fn);
        match isolation {
            IsolationType::Tdx => {
                assert_eq!(caps.vtom.is_some(), !hide_isolation);
                // TDX 1.5 requires EFER.NXE to be set to 1, so set it at RESET/INIT.
                caps.nxe_forced_on = true;
            }
            IsolationType::Snp => {
                assert_eq!(caps.vtom.is_some(), !hide_isolation);
            }
            _ => {
                assert!(caps.vtom.is_none());
            }
        }

        caps
    }

    /// Forward a (virtual) MMIO read to the host for handling.
    pub fn host_mmio_read(&self, addr: u64, data: &mut [u8]) {
        if !self.inner.use_mmio_hypercalls {
            return;
        }
        // There isn't anything reasonable that can be done in the face of errors from the host.
        if let Err(err) = self.inner.hcl.memory_mapped_io_read(addr, data) {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "Failed host MMIO read"
            );
        }
    }

    /// Forward a (virtual) MMIO write to the host for handling.
    pub fn host_mmio_write(&self, addr: u64, data: &[u8]) {
        if !self.inner.use_mmio_hypercalls {
            return;
        }
        // There isn't anything reasonable that can be done in the face of errors from the host.
        if let Err(err) = self.inner.hcl.memory_mapped_io_write(addr, data) {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "Failed host MMIO write"
            );
        }
    }
}

#[cfg(guest_arch = "x86_64")]
/// Gets the TSC frequency for the current platform.
fn get_tsc_frequency(isolation: IsolationType) -> Result<u64, Error> {
    // Always get the frequency from the hypervisor. It's believed that, as long
    // as the hypervisor is behaving, it will provide the most precise and accurate frequency.
    let msr = MsrDevice::new(0).map_err(Error::OpenMsr)?;
    let hv_frequency = msr
        .read_msr(hvdef::HV_X64_MSR_TSC_FREQUENCY)
        .map_err(Error::ReadTscFrequency)?;

    // Get the hardware-advertised frequency and validate that the
    // hypervisor frequency is not too far off.
    let hw_info = match isolation {
        IsolationType::Tdx => {
            // TDX provides the TSC frequency via cpuid.
            let max_function =
                safe_intrinsics::cpuid(x86defs::cpuid::CpuidFunction::VendorAndMaxFunction.0, 0)
                    .eax;

            if max_function < x86defs::cpuid::CpuidFunction::CoreCrystalClockInformation.0 {
                return Err(Error::BadCpuidTsc);
            }
            let result = safe_intrinsics::cpuid(
                x86defs::cpuid::CpuidFunction::CoreCrystalClockInformation.0,
                0,
            );
            let ratio_denom = result.eax;
            let ratio_num = result.ebx;
            let clock = result.ecx;
            if ratio_num == 0 || ratio_denom == 0 || clock == 0 {
                return Err(Error::BadCpuidTsc);
            }
            // TDX TSC is configurable in units of 25MHz, so allow up to 12.5MHz
            // error.
            let allowed_error = 12_500_000;
            Some((
                clock as u64 * ratio_num as u64 / ratio_denom as u64,
                allowed_error,
            ))
        }
        IsolationType::Snp => {
            // SNP currently does not provide the frequency.
            None
        }
        IsolationType::Vbs | IsolationType::None => None,
    };

    if let Some((hw_frequency, allowed_error)) = hw_info {
        // Don't allow the frequencies to be different by more than the hardware
        // precision.
        let delta = hw_frequency.abs_diff(hv_frequency);
        if delta > allowed_error {
            return Err(Error::TscFrequencyMismatch {
                hv: hv_frequency,
                hw: hw_frequency,
                allowed_error,
            });
        }
    }

    Ok(hv_frequency)
}

impl UhPartitionInner {
    fn manage_io_port_intercept_region(&self, begin: u16, end: u16, active: bool) {
        if self.isolation.is_hardware_isolated() {
            return;
        }

        static SKIP_RANGE: AtomicBool = AtomicBool::new(false);

        let access_type_mask = if active {
            HV_INTERCEPT_ACCESS_MASK_READ_WRITE
        } else {
            HV_INTERCEPT_ACCESS_MASK_NONE
        };

        // Try to register the whole range at once.
        if !SKIP_RANGE.load(Ordering::Relaxed) {
            match self.hcl.register_intercept(
                HvInterceptType::HvInterceptTypeX64IoPortRange,
                access_type_mask,
                HvInterceptParameters::new_io_port_range(begin..=end),
            ) {
                Ok(()) => return,
                Err(HvError::InvalidParameter) => {
                    // Probably a build that doesn't support range wrapping yet.
                    // Don't try again.
                    SKIP_RANGE.store(true, Ordering::Relaxed);
                    tracing::warn!("old hypervisor build; using slow path for intercept ranges");
                }
                Err(err) => {
                    panic!("io port range registration failure: {err:?}");
                }
            }
        }

        // Fall back to registering one port at a time.
        for port in begin..=end {
            self.hcl
                .register_intercept(
                    HvInterceptType::HvInterceptTypeX64IoPort,
                    access_type_mask,
                    HvInterceptParameters::new_io_port(port),
                )
                .expect("registering io intercept cannot fail");
        }
    }

    fn is_gpa_lower_vtl_ram(&self, gpa: u64) -> bool {
        // TODO: this probably should reflect changes to the memory map via PAM
        // registers. Right now this isn't an issue because the relevant region,
        // VGA, is handled on the host.
        self.lower_vtl_memory_layout
            .ram()
            .iter()
            .any(|m| m.range.contains_addr(gpa))
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        // TODO: this probably should reflect changes to the memory map via PAM
        // registers. Right now this isn't an issue because the relevant region,
        // VGA, is handled on the host.
        if self.is_gpa_lower_vtl_ram(gpa) {
            // The monitor page is protected against lower VTL writes.
            !write || self.monitor_page.gpa() != Some(gpa & !(HV_PAGE_SIZE - 1))
        } else {
            false
        }
    }
}

/// Handle returned by [`UhPartition::register_host_io_port_fast_path`].
///
/// When dropped, unregisters the IO ports so that they are no longer forwarded
/// to the host.
#[must_use]
pub struct HostIoPortFastPathHandle {
    inner: Weak<UhPartitionInner>,
    begin: u16,
    end: u16,
}

impl Drop for HostIoPortFastPathHandle {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.manage_io_port_intercept_region(self.begin, self.end, true);
        }
    }
}

/// The application level VTL crash data not suited for putting
/// on the wire.
///
/// FUTURE: move/remove this to standardize across virt backends.
#[derive(Copy, Clone, Debug)]
pub struct VtlCrash {
    /// The VP that crashed.
    pub vp_index: VpIndex,
    /// The VTL that crashed.
    pub last_vtl: GuestVtl,
    /// The crash control information.
    pub control: GuestCrashCtl,
    /// The crash parameters.
    pub parameters: [u64; 5],
}

/// Validate that flags is a valid setting for VTL memory protection when
/// applied to VTL 1.
pub fn validate_vtl_gpa_flags(
    flags: HvMapGpaFlags,
    mbec_enabled: bool,
    shadow_supervisor_stack_enabled: bool,
) -> bool {
    // Adjust is not allowed for VTL1.
    if flags.adjustable() {
        return false;
    }

    // KX must equal UX unless MBEC is enabled. KX && !UX is invalid.
    if flags.kernel_executable() != flags.user_executable() {
        if (flags.kernel_executable() && !flags.user_executable()) || !mbec_enabled {
            return false;
        }
    }

    // Read must be specified if anything else is specified.
    if flags.writable()
        || flags.kernel_executable()
        || flags.user_executable()
        || flags.supervisor_shadow_stack()
        || flags.paging_writability()
        || flags.verify_paging_writability()
    {
        if !flags.readable() {
            return false;
        }
    }

    // Supervisor shadow stack protection is invalid if shadow stacks are disabled
    // or if execute is not specified.
    if flags.supervisor_shadow_stack()
        && ((!flags.kernel_executable() && !flags.user_executable())
            || shadow_supervisor_stack_enabled)
    {
        return false;
    }

    true
}
