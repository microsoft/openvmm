// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the Underhill hypervisor backend, which uses
//! `/dev/mshv_vtl` to interact with the Microsoft hypervisor while running in
//! VTL2.

#![cfg(all(guest_is_native, target_os = "linux"))]

mod devmsr;

cfg_if::cfg_if!(
    if #[cfg(guest_arch = "x86_64")] {
        mod cvm_cpuid;
        pub use processor::snp::SnpBacked;
        pub use processor::tdx::TdxBacked;
        use crate::processor::HardwareIsolatedBacking;
        pub use crate::processor::mshv::x64::HypervisorBackedX86 as HypervisorBacked;
        use crate::processor::mshv::x64::HypervisorBackedX86Shared as HypervisorBackedShared;
        use bitvec::prelude::BitArray;
        use bitvec::prelude::Lsb0;
        use devmsr::MsrDevice;
        use hv1_emulator::hv::ProcessorVtlHv;
        use processor::LapicState;
        use processor::snp::SnpBackedShared;
        use processor::tdx::TdxBackedShared;
        use std::arch::x86_64::CpuidResult;
        use virt::CpuidLeaf;
        use virt::state::StateElement;
        use virt::vp::MpState;
        /// Bitarray type for representing IRR bits in a x86-64 APIC
        /// Each bit represent the 256 possible vectors.
        type IrrBitmap = BitArray<[u32; 8], Lsb0>;
    } else if #[cfg(guest_arch = "aarch64")] {
        pub use crate::processor::mshv::arm64::HypervisorBackedArm64 as HypervisorBacked;
        use crate::processor::mshv::arm64::HypervisorBackedArm64Shared as HypervisorBackedShared;
    }
);

mod processor;
pub use processor::Backing;
pub use processor::UhProcessor;

use anyhow::Context as AnyhowContext;
use bitfield_struct::bitfield;
use bitvec::boxed::BitBox;
use bitvec::vec::BitVec;
use cvm_tracing::CVM_ALLOWED;
use guestmem::GuestMemory;
use guestmem::GuestMemoryBackingError;
use hcl::GuestVtl;
use hcl::ioctl::Hcl;
use hcl::ioctl::SetVsmPartitionConfigError;
use hv1_emulator::hv::GlobalHv;
use hv1_emulator::message_queues::MessageQueues;
use hv1_emulator::synic::GlobalSynic;
use hv1_emulator::synic::SintProxied;
use hv1_structs::VtlArray;
use hvdef::GuestCrashCtl;
use hvdef::HV_PAGE_SHIFT;
use hvdef::HV_PAGE_SIZE;
use hvdef::HV_PAGE_SIZE_USIZE;
use hvdef::HvError;
use hvdef::HvMapGpaFlags;
use hvdef::HvPartitionPrivilege;
use hvdef::HvRegisterName;
use hvdef::HvRegisterVsmPartitionConfig;
use hvdef::HvRegisterVsmPartitionStatus;
use hvdef::Vtl;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_EXECUTE;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_NONE;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_READ_WRITE;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_WRITE;
use hvdef::hypercall::HostVisibilityType;
use hvdef::hypercall::HvGuestOsId;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::HvInterceptParameters;
use hvdef::hypercall::HvInterceptType;
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
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Waker;
use thiserror::Error;
use user_driver::DmaClient;
use virt::IsolationType;
use virt::PartitionCapabilities;
use virt::VpIndex;
use virt::X86Partition;
use virt::irqcon::IoApicRouting;
use virt::irqcon::MsiRequest;
use virt::x86::apic_software_device::ApicSoftwareDevices;
use virt_support_apic::LocalApicSet;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::TargetVpInfo;
use vmcore::monitor::MonitorPage;
use vmcore::reference_time::GetReferenceTime;
use vmcore::reference_time::ReferenceTimeResult;
use vmcore::reference_time::ReferenceTimeSource;
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
    AllocateSharedVisOverlay(#[source] anyhow::Error),
    #[error("failed to open msr device")]
    OpenMsr(#[source] std::io::Error),
    #[error("cpuid did not contain valid TSC frequency information")]
    BadCpuidTsc,
    #[error("failed to read tsc frequency")]
    ReadTscFrequency(#[source] std::io::Error),
    #[error(
        "tsc frequency mismatch between hypervisor ({hv}) and hardware {hw}, exceeds allowed error {allowed_error}"
    )]
    TscFrequencyMismatch {
        hv: u64,
        hw: u64,
        allowed_error: u64,
    },
    #[error("failed to set vsm partition config: {0:?}")]
    FailedToSetL2Ctls(TdCallResult),
    #[error("debugging is configured but the binary does not have the gdb feature")]
    InvalidDebugConfiguration,
    #[error("failed to allocate TLB flush page")]
    AllocateTlbFlushPage(#[source] anyhow::Error),
    #[error("host does not support required cpu capabilities")]
    Capabilities(virt::PartitionCapabilitiesError),
    #[error("failed to get register")]
    GetReg(#[source] hcl::ioctl::register::GetRegError),
    #[error("failed to set register")]
    SetReg(#[source] hcl::ioctl::register::SetRegError),
}

/// Error revoking guest VSM.
#[derive(Error, Debug)]
#[expect(missing_docs)]
pub enum RevokeGuestVsmError {
    #[error("failed to set vsm config")]
    SetGuestVsmConfig(#[source] hcl::ioctl::register::SetRegError),
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

/// Underhill partition.
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
    #[cfg(guest_arch = "x86_64")]
    cpuid: virt::CpuidLeafSet,
    lower_vtl_memory_layout: MemoryLayout,
    gm: VtlArray<GuestMemory, 2>,
    vtl0_kernel_exec_gm: GuestMemory,
    vtl0_user_exec_gm: GuestMemory,
    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
    #[inspect(skip)]
    crash_notification_send: mesh::Sender<VtlCrash>,
    monitor_page: MonitorPage,
    #[inspect(skip)]
    allocated_monitor_page: Mutex<Option<user_driver::memory::MemoryBlock>>,
    software_devices: Option<ApicSoftwareDevices>,
    #[inspect(skip)]
    vmtime: VmTimeSource,
    isolation: IsolationType,
    #[inspect(with = "inspect::AtomicMut")]
    no_sidecar_hotplug: AtomicBool,
    use_mmio_hypercalls: bool,
    backing_shared: BackingShared,
    intercept_debug_exceptions: bool,
    #[cfg(guest_arch = "x86_64")]
    // N.B For now, only one device vector table i.e. for VTL0 only
    #[inspect(hex, with = "|x| inspect::iter_by_index(x.read().into_inner())")]
    device_vector_table: RwLock<IrrBitmap>,
    vmbus_relay: bool,
}

#[derive(Inspect)]
#[inspect(untagged)]
enum BackingShared {
    Hypervisor(#[inspect(flatten)] HypervisorBackedShared),
    #[cfg(guest_arch = "x86_64")]
    Snp(#[inspect(flatten)] SnpBackedShared),
    #[cfg(guest_arch = "x86_64")]
    Tdx(#[inspect(flatten)] TdxBackedShared),
}

impl BackingShared {
    fn new(
        isolation: IsolationType,
        partition_params: &UhPartitionNewParams<'_>,
        backing_shared_params: BackingSharedParams<'_>,
    ) -> Result<BackingShared, Error> {
        Ok(match isolation {
            IsolationType::None | IsolationType::Vbs => {
                assert!(backing_shared_params.cvm_state.is_none());
                BackingShared::Hypervisor(HypervisorBackedShared::new(
                    partition_params,
                    backing_shared_params,
                )?)
            }
            #[cfg(guest_arch = "x86_64")]
            IsolationType::Snp => BackingShared::Snp(SnpBackedShared::new(
                partition_params,
                backing_shared_params,
            )?),
            #[cfg(guest_arch = "x86_64")]
            IsolationType::Tdx => BackingShared::Tdx(TdxBackedShared::new(
                partition_params,
                backing_shared_params,
            )?),
            #[cfg(not(guest_arch = "x86_64"))]
            _ => unreachable!(),
        })
    }

    fn cvm_state(&self) -> Option<&UhCvmPartitionState> {
        match self {
            BackingShared::Hypervisor(_) => None,
            #[cfg(guest_arch = "x86_64")]
            BackingShared::Snp(SnpBackedShared { cvm, .. })
            | BackingShared::Tdx(TdxBackedShared { cvm, .. }) => Some(cvm),
        }
    }

    fn untrusted_synic(&self) -> Option<&GlobalSynic> {
        match self {
            BackingShared::Hypervisor(_) => None,
            #[cfg(guest_arch = "x86_64")]
            BackingShared::Snp(_) => None,
            #[cfg(guest_arch = "x86_64")]
            BackingShared::Tdx(s) => s.untrusted_synic.as_ref(),
        }
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
struct GuestVsmVpState {
    /// The pending event that VTL 1 wants to inject into VTL 0. Injected on
    /// next exit to VTL 0.
    #[inspect(with = "|x| x.as_ref().map(inspect::AsDebug)")]
    vtl0_exit_pending_event: Option<hvdef::HvX64PendingExceptionEvent>,
    reg_intercept: SecureRegisterInterceptState,
}

#[cfg(guest_arch = "x86_64")]
impl GuestVsmVpState {
    fn new() -> Self {
        GuestVsmVpState {
            vtl0_exit_pending_event: None,
            reg_intercept: Default::default(),
        }
    }
}

#[cfg(guest_arch = "x86_64")]
#[derive(Inspect)]
/// VP state for CVMs.
struct UhCvmVpState {
    // Allocation handle for direct overlays
    #[inspect(debug)]
    direct_overlay_handle: user_driver::memory::MemoryBlock,
    /// Used in VTL 2 exit code to determine which VTL to exit to.
    exit_vtl: GuestVtl,
    /// Hypervisor enlightenment emulator state.
    hv: VtlArray<ProcessorVtlHv, 2>,
    /// LAPIC state.
    lapics: VtlArray<LapicState, 2>,
    /// Guest VSM state for this vp. Some when VTL 1 is enabled.
    vtl1: Option<GuestVsmVpState>,
}

#[cfg(guest_arch = "x86_64")]
impl UhCvmVpState {
    /// Creates a new CVM VP state.
    pub(crate) fn new(
        cvm_partition: &UhCvmPartitionState,
        inner: &UhPartitionInner,
        vp_info: &TargetVpInfo,
        overlay_pages_required: usize,
    ) -> Result<Self, Error> {
        let direct_overlay_handle = cvm_partition
            .shared_dma_client
            .allocate_dma_buffer(overlay_pages_required * HV_PAGE_SIZE as usize)
            .map_err(Error::AllocateSharedVisOverlay)?;

        let apic_base = virt::vp::Apic::at_reset(&inner.caps, vp_info).apic_base;
        let lapics = VtlArray::from_fn(|vtl| {
            let apic_set = &cvm_partition.lapic[vtl];

            // The APIC is software-enabled after reset for secure VTLs, to
            // maintain compatibility with released versions of secure kernel
            let mut lapic = apic_set.add_apic(vp_info, vtl == Vtl::Vtl1);
            // Initialize APIC base to match the reset VM state.
            lapic.set_apic_base(apic_base).unwrap();
            // Only the VTL 0 non-BSP LAPICs should be in the WaitForSipi state.
            let activity = if vtl == Vtl::Vtl0 && !vp_info.base.is_bsp() {
                MpState::WaitForSipi
            } else {
                MpState::Running
            };
            LapicState::new(lapic, activity)
        });

        let hv = VtlArray::from_fn(|vtl| cvm_partition.hv.add_vp(vp_info.base.vp_index, vtl));

        Ok(Self {
            direct_overlay_handle,
            exit_vtl: GuestVtl::Vtl0,
            hv,
            lapics,
            vtl1: None,
        })
    }
}

#[cfg(guest_arch = "x86_64")]
#[derive(Inspect, Default)]
#[inspect(hex)]
/// Configuration of VTL 1 registration for intercepts on certain registers
pub struct SecureRegisterInterceptState {
    #[inspect(with = "|&x| u64::from(x)")]
    intercept_control: hvdef::HvRegisterCrInterceptControl,
    cr0_mask: u64,
    cr4_mask: u64,
    // Writes to X86X_IA32_MSR_MISC_ENABLE are dropped, so this is only used so
    // that get_vp_register returns the correct value from a set_vp_register
    ia32_misc_enable_mask: u64,
}

/// Information about a redirected interrupt for a specific vector.
/// Stored per-processor, indexed by the redirected vector number in VTL2.
#[derive(Clone, Inspect)]
struct ProxyRedirectVectorInfo {
    /// Device ID that owns this interrupt
    device_id: u64,
    /// Original interrupt vector from the device
    original_vector: u32,
}

#[derive(Inspect)]
/// Partition-wide state for CVMs.
struct UhCvmPartitionState {
    #[cfg(guest_arch = "x86_64")]
    vps_per_socket: u32,
    /// VPs that have locked their TLB.
    #[inspect(
        with = "|arr| inspect::iter_by_index(arr.iter()).map_value(|bb| inspect::iter_by_index(bb.iter().map(|v| *v)))"
    )]
    tlb_locked_vps: VtlArray<BitBox<AtomicU64>, 2>,
    #[inspect(with = "inspect::iter_by_index")]
    vps: Vec<UhCvmVpInner>,
    shared_memory: GuestMemory,
    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
    #[inspect(skip)]
    isolated_memory_protector: Arc<dyn ProtectIsolatedMemory>,
    /// The emulated local APIC set.
    lapic: VtlArray<LocalApicSet, 2>,
    /// The emulated hypervisor state.
    hv: GlobalHv<2>,
    /// Guest VSM state.
    guest_vsm: RwLock<GuestVsmState<CvmVtl1State>>,
    /// Dma client for shared visibility pages.
    shared_dma_client: Arc<dyn DmaClient>,
    /// Dma client for private visibility pages.
    private_dma_client: Arc<dyn DmaClient>,
    hide_isolation: bool,
    proxy_interrupt_redirect: bool,
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
impl UhCvmPartitionState {
    fn vp_inner(&self, vp_index: u32) -> &UhCvmVpInner {
        &self.vps[vp_index as usize]
    }

    fn is_lower_vtl_startup_denied(&self) -> bool {
        matches!(
            *self.guest_vsm.read(),
            GuestVsmState::Enabled {
                vtl1: CvmVtl1State {
                    deny_lower_vtl_startup: true,
                    ..
                }
            }
        )
    }
}

#[derive(Inspect)]
/// Per-vp state for CVMs.
struct UhCvmVpInner {
    /// The current status of TLB locks
    tlb_lock_info: VtlArray<TlbLockInfo, 2>,
    /// Whether EnableVpVtl for VTL 1 has been called on this VP.
    vtl1_enable_called: Mutex<bool>,
    /// Whether the VP has been started via the StartVp hypercall.
    started: AtomicBool,
    /// Start context for StartVp and EnableVpVtl calls.
    #[inspect(with = "|arr| inspect::iter_by_index(arr.iter().map(|v| v.lock().is_some()))")]
    hv_start_enable_vtl_vp: VtlArray<Mutex<Option<Box<VpStartEnableVtl>>>, 2>,
    /// Tracking of proxy redirect interrupts mapped on this VP.
    #[inspect(with = "|x| inspect::adhoc(|req| inspect::iter_by_key(&*x.lock()).inspect(req))")]
    proxy_redirect_interrupts: Mutex<HashMap<u32, ProxyRedirectVectorInfo>>,
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
#[derive(Inspect)]
#[inspect(tag = "guest_vsm_state")]
/// Partition-wide state for guest vsm.
enum GuestVsmState<T: Inspect> {
    NotPlatformSupported,
    NotGuestEnabled,
    Enabled {
        #[inspect(flatten)]
        vtl1: T,
    },
}

impl<T: Inspect> GuestVsmState<T> {
    pub fn from_availability(guest_vsm_available: bool) -> Self {
        if guest_vsm_available {
            GuestVsmState::NotGuestEnabled
        } else {
            GuestVsmState::NotPlatformSupported
        }
    }
}

#[derive(Inspect)]
struct CvmVtl1State {
    /// Whether VTL 1 has been enabled on any vp
    enabled_on_any_vp: bool,
    /// Whether guest memory should be zeroed before it resets.
    zero_memory_on_reset: bool,
    /// Whether a vp can be started or reset by a lower vtl.
    deny_lower_vtl_startup: bool,
    /// Whether Mode-Based Execution Control should be enforced on lower VTLs.
    pub mbec_enabled: bool,
    /// Whether shadow supervisor stack is enabled.
    pub shadow_supervisor_stack_enabled: bool,
    #[inspect(with = "|bb| inspect::iter_by_index(bb.iter().map(|v| *v))")]
    io_read_intercepts: BitBox<u64>,
    #[inspect(with = "|bb| inspect::iter_by_index(bb.iter().map(|v| *v))")]
    io_write_intercepts: BitBox<u64>,
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
impl CvmVtl1State {
    fn new(mbec_enabled: bool) -> Self {
        Self {
            enabled_on_any_vp: false,
            zero_memory_on_reset: false,
            deny_lower_vtl_startup: false,
            mbec_enabled,
            shadow_supervisor_stack_enabled: false,
            io_read_intercepts: BitVec::repeat(false, u16::MAX as usize + 1).into_boxed_bitslice(),
            io_write_intercepts: BitVec::repeat(false, u16::MAX as usize + 1).into_boxed_bitslice(),
        }
    }
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
struct TscReferenceTimeSource {
    tsc_scale: u64,
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
impl TscReferenceTimeSource {
    fn new(tsc_frequency: u64) -> Self {
        TscReferenceTimeSource {
            tsc_scale: (((10_000_000_u128) << 64) / tsc_frequency as u128) as u64,
        }
    }
}

/// A time implementation based on TSC.
impl GetReferenceTime for TscReferenceTimeSource {
    fn now(&self) -> ReferenceTimeResult {
        #[cfg(guest_arch = "x86_64")]
        {
            let tsc = safe_intrinsics::rdtsc();
            let ref_time = ((self.tsc_scale as u128 * tsc as u128) >> 64) as u64;
            ReferenceTimeResult {
                ref_time,
                system_time: None,
            }
        }

        #[cfg(guest_arch = "aarch64")]
        {
            todo!("AARCH64_TODO");
        }
    }
}

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
    /// The Linux kernel's CPU index for this VP. This should be used instead of VpIndex
    /// when interacting with non-MSHV kernel interfaces.
    cpu_index: u32,
    sidecar_exit_reason: Mutex<Option<SidecarExitReason>>,
}

impl UhVpInner {
    pub fn vp_index(&self) -> VpIndex {
        self.vp_info.base.vp_index
    }
}

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
#[derive(Debug, Inspect)]
/// Which operation is setting the initial vp context
enum InitialVpContextOperation {
    /// The VP is being started via the StartVp hypercall.
    StartVp,
    /// The VP is being started via the EnableVpVtl hypercall.
    EnableVpVtl,
}

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
#[derive(Debug, Inspect)]
/// State for handling StartVp/EnableVpVtl hypercalls.
struct VpStartEnableVtl {
    /// Which operation, startvp or enablevpvtl, is setting the initial vp
    /// context
    operation: InitialVpContextOperation,
    #[inspect(skip)]
    context: hvdef::hypercall::InitialVpContextX64,
}

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

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
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

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ExitActivity {
    pending_event: bool,
    #[bits(31)]
    _reserved: u32,
}

/// Immutable access to useful bits of Partition state.
impl UhPartition {
    /// Revokes guest VSM.
    pub fn revoke_guest_vsm(&self) -> Result<(), RevokeGuestVsmError> {
        fn revoke<T: Inspect>(vsm_state: &mut GuestVsmState<T>) -> Result<(), RevokeGuestVsmError> {
            if matches!(vsm_state, GuestVsmState::Enabled { .. }) {
                return Err(RevokeGuestVsmError::Vtl1AlreadyEnabled);
            }
            *vsm_state = GuestVsmState::NotPlatformSupported;
            Ok(())
        }

        match &self.inner.backing_shared {
            BackingShared::Hypervisor(s) => {
                revoke(&mut *s.guest_vsm.write())?;
                self.inner
                    .hcl
                    .set_guest_vsm_partition_config(false)
                    .map_err(RevokeGuestVsmError::SetGuestVsmConfig)?;
            }
            #[cfg(guest_arch = "x86_64")]
            BackingShared::Snp(SnpBackedShared { cvm, .. })
            | BackingShared::Tdx(TdxBackedShared { cvm, .. }) => {
                revoke(&mut *cvm.guest_vsm.write())?;
            }
        };

        Ok(())
    }

    /// Returns the current hypervisor reference time, in 100ns units.
    pub fn reference_time(&self) -> u64 {
        if let Some(hv) = self.inner.hv() {
            hv.ref_time_source().now().ref_time
        } else {
            self.inner
                .hcl
                .reference_time()
                .expect("should not fail to get the reference time")
        }
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

impl X86Partition for UhPartition {
    fn ioapic_routing(&self) -> Arc<dyn IoApicRouting> {
        self.inner.clone()
    }

    fn pulse_lint(&self, vp_index: VpIndex, vtl: Vtl, lint: u8) {
        let vtl = GuestVtl::try_from(vtl).expect("higher vtl not configured");
        if let Some(apic) = &self.inner.lapic(vtl) {
            apic.lint(vp_index, lint.into(), |vp_index| {
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

    fn lapic(&self, vtl: GuestVtl) -> Option<&LocalApicSet> {
        self.backing_shared.cvm_state().map(|x| &x.lapic[vtl])
    }

    fn hv(&self) -> Option<&GlobalHv<2>> {
        self.backing_shared.cvm_state().map(|x| &x.hv)
    }

    /// For requester VP to issue `proxy_irr_blocked` update to other VPs
    #[cfg(guest_arch = "x86_64")]
    fn request_proxy_irr_filter_update(
        &self,
        vtl: GuestVtl,
        device_vector: u8,
        req_vp_index: VpIndex,
    ) {
        tracing::debug!(
            ?vtl,
            device_vector,
            req_vp_index = req_vp_index.index(),
            "request_proxy_irr_filter_update"
        );

        // Add given vector to partition global device vector table (VTL0 only for now)
        {
            let mut device_vector_table = self.device_vector_table.write();
            device_vector_table.set(device_vector as usize, true);
        }

        // Wake all other VPs for their `proxy_irr_blocked` filter update
        for vp in self.vps.iter() {
            if vp.vp_index() != req_vp_index {
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
    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
    fn vsm_status(
        &self,
    ) -> Result<HvRegisterVsmPartitionStatus, hcl::ioctl::register::GetRegError> {
        // TODO: It might be possible to cache VsmPartitionStatus.
        self.hcl.get_vsm_partition_status()
    }
}

impl virt::Synic for UhPartition {
    fn post_message(&self, vtl: Vtl, vp_index: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        let vtl = GuestVtl::try_from(vtl).expect("higher vtl not configured");
        let Some(vp) = self.inner.vp(vp_index) else {
            tracelimit::warn_ratelimited!(
                CVM_ALLOWED,
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

    fn new_guest_event_port(
        &self,
        vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn vmcore::synic::GuestEventPort> {
        let vtl = GuestVtl::try_from(vtl).expect("higher vtl not configured");
        Box::new(UhEventPort {
            partition: Arc::downgrade(&self.inner),
            params: Arc::new(Mutex::new(UhEventPortParams {
                vp: VpIndex::new(vp),
                sint,
                flag,
                vtl,
            })),
        })
    }

    fn prefer_os_events(&self) -> bool {
        false
    }

    fn monitor_support(&self) -> Option<&dyn virt::SynicMonitor> {
        Some(self)
    }
}

impl virt::SynicMonitor for UhPartition {
    fn set_monitor_page(&self, vtl: Vtl, gpa: Option<u64>) -> anyhow::Result<()> {
        // Keep this locked the whole function to avoid racing with allocate_monitor_page.
        let mut allocated_block = self.inner.allocated_monitor_page.lock();
        let old_gpa = self.inner.monitor_page.set_gpa(gpa);

        // Take ownership of any allocated monitor page so it will be freed on function exit.
        let allocated_page = allocated_block.take();
        if let Some(old_gpa) = old_gpa {
            let allocated_gpa = allocated_page
                .as_ref()
                .map(|b| b.pfns()[0] << HV_PAGE_SHIFT);

            // Revert the old page's permissions, using the appropriate method depending on
            // whether it was allocated or guest-supplied.
            let result = if allocated_gpa == Some(old_gpa) {
                let vtl = GuestVtl::try_from(vtl).unwrap();
                self.unregister_cvm_dma_overlay_page(vtl, old_gpa >> HV_PAGE_SHIFT)
            } else {
                self.inner
                    .hcl
                    .modify_vtl_protection_mask(
                        MemoryRange::new(old_gpa..old_gpa + HV_PAGE_SIZE),
                        hvdef::HV_MAP_GPA_PERMISSIONS_ALL,
                        HvInputVtl::CURRENT_VTL,
                    )
                    .map_err(|err| anyhow::anyhow!(err))
            };

            result
                .context("failed to unregister old monitor page")
                .inspect_err(|_| {
                    // Leave the page unset if returning a failure.
                    self.inner.monitor_page.set_gpa(None);
                })?;

            tracing::debug!(old_gpa, "unregistered monitor page");
        }

        if let Some(gpa) = gpa {
            // Disallow VTL0 from writing to the page, so we'll get an intercept. Note that read
            // permissions must be enabled or this doesn't work correctly.
            self.inner
                .hcl
                .modify_vtl_protection_mask(
                    MemoryRange::new(gpa..gpa + HV_PAGE_SIZE),
                    HvMapGpaFlags::new().with_readable(true),
                    HvInputVtl::CURRENT_VTL,
                )
                .context("failed to register monitor page")
                .inspect_err(|_| {
                    // Leave the page unset if returning a failure.
                    self.inner.monitor_page.set_gpa(None);
                })?;

            tracing::debug!(gpa, "registered monitor page");
        }

        Ok(())
    }

    fn register_monitor(
        &self,
        monitor_id: vmcore::monitor::MonitorId,
        connection_id: u32,
    ) -> Box<dyn Sync + Send> {
        self.inner
            .monitor_page
            .register_monitor(monitor_id, connection_id)
    }

    fn allocate_monitor_page(&self, vtl: Vtl) -> anyhow::Result<Option<u64>> {
        let vtl = GuestVtl::try_from(vtl).unwrap();

        // Allocating a monitor page is only supported for CVMs.
        let Some(state) = self.inner.backing_shared.cvm_state() else {
            return Ok(None);
        };

        let mut allocated_block = self.inner.allocated_monitor_page.lock();
        if let Some(block) = allocated_block.as_ref() {
            // An allocated monitor page is already in use; no need to change it.
            let gpa = block.pfns()[0] << HV_PAGE_SHIFT;
            assert_eq!(self.inner.monitor_page.gpa(), Some(gpa));
            return Ok(Some(gpa));
        }

        let block = state
            .private_dma_client
            .allocate_dma_buffer(HV_PAGE_SIZE_USIZE)
            .context("failed to allocate monitor page")?;

        let gpn = block.pfns()[0];
        *allocated_block = Some(block);
        let gpa = gpn << HV_PAGE_SHIFT;
        let old_gpa = self.inner.monitor_page.set_gpa(Some(gpa));
        if let Some(old_gpa) = old_gpa {
            // The old GPA is guaranteed not to be allocated, since that was checked above, so
            // revert its permissions using the method for guest-supplied memory.
            self.inner
                .hcl
                .modify_vtl_protection_mask(
                    MemoryRange::new(old_gpa..old_gpa + HV_PAGE_SIZE),
                    hvdef::HV_MAP_GPA_PERMISSIONS_ALL,
                    HvInputVtl::CURRENT_VTL,
                )
                .context("failed to unregister old monitor page")
                .inspect_err(|_| {
                    // Leave the page unset if returning a failure.
                    self.inner.monitor_page.set_gpa(None);
                })?;

            tracing::debug!(old_gpa, "unregistered monitor page");
        }

        // Disallow VTL0 from writing to the page, so we'll get an intercept. Note that read
        // permissions must be enabled or this doesn't work correctly.
        self.register_cvm_dma_overlay_page(vtl, gpn, HvMapGpaFlags::new().with_readable(true))
            .context("failed to unregister monitor page")
            .inspect_err(|_| {
                // Leave the page unset if returning a failure.
                self.inner.monitor_page.set_gpa(None);
            })?;

        tracing::debug!(gpa, "registered allocated monitor page");

        Ok(Some(gpa))
    }
}

impl UhPartitionInner {
    #[cfg(guest_arch = "x86_64")]
    pub(crate) fn synic_interrupt(
        &self,
        vp_index: VpIndex,
        vtl: GuestVtl,
    ) -> impl '_ + hv1_emulator::RequestInterrupt {
        // TODO CVM: optimize for SNP with secure avic to avoid internal wake
        // and for TDX to avoid trip to user mode
        move |vector, auto_eoi| {
            self.lapic(vtl).unwrap().synic_interrupt(
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
    params: Arc<Mutex<UhEventPortParams>>,
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
            let UhEventPortParams {
                vp,
                sint,
                flag,
                vtl,
            } = *params.lock();
            let Some(partition) = partition.upgrade() else {
                return;
            };
            tracing::trace!(vp = vp.index(), sint, flag, "signal_event");
            if let Some(hv) = partition.hv() {
                match hv.synic[vtl].signal_event(
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
                        if let Some(synic) = partition.backing_shared.untrusted_synic() {
                            synic
                                .signal_event(
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

    fn set_target_vp(&mut self, vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
        self.params.lock().vp = VpIndex::new(vp);
        Ok(())
    }
}

impl virt::Hv1 for UhPartition {
    type Error = Error;
    type Device = virt::x86::apic_software_device::ApicSoftwareDevice;

    fn reference_time_source(&self) -> Option<ReferenceTimeSource> {
        Some(if let Some(hv) = self.inner.hv() {
            hv.ref_time_source().clone()
        } else {
            ReferenceTimeSource::from(self.inner.clone() as Arc<_>)
        })
    }

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        self.inner.software_devices.is_some().then_some(self)
    }
}

impl GetReferenceTime for UhPartitionInner {
    fn now(&self) -> ReferenceTimeResult {
        ReferenceTimeResult {
            ref_time: self.hcl.reference_time().unwrap(),
            system_time: None,
        }
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
        if let Some(lapic) = self.lapic(vtl) {
            tracing::trace!(?request, "interrupt");
            lapic.request_interrupt(request.address, request.data, |vp_index| {
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
                    CVM_ALLOWED,
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
fn set_vtl2_vsm_partition_config(hcl: &Hcl) -> Result<(), Error> {
    // Read available capabilities to determine what to enable.
    let caps = hcl.get_vsm_capabilities().map_err(Error::GetReg)?;
    let hardware_isolated = hcl.isolation().is_hardware_isolated();
    let isolated = hcl.isolation().is_isolated();
    // xtask-fmt allow-target-arch cpu-intrinsic
    let restore_partition_time_available = if cfg!(target_arch = "x86_64") {
        let result =
            safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION, 0);
        hvdef::HvEnlightenmentInformation::from(
            result.eax as u128
                | (result.ebx as u128) << 32
                | (result.ecx as u128) << 64
                | (result.edx as u128) << 96,
        )
        .restore_time_on_resume()
    } else {
        // Only available on x86_64 hypervisors.
        false
    };

    let config = HvRegisterVsmPartitionConfig::new()
        .with_default_vtl_protection_mask(0xF)
        .with_enable_vtl_protection(!hardware_isolated)
        .with_zero_memory_on_reset(!hardware_isolated)
        .with_intercept_cpuid_unimplemented(!hardware_isolated)
        .with_intercept_page(caps.intercept_page_available())
        .with_intercept_unrecoverable_exception(true)
        .with_intercept_not_present(caps.intercept_not_present_available() && !isolated)
        .with_intercept_acceptance(isolated)
        .with_intercept_enable_vtl_protection(isolated && !hardware_isolated)
        .with_intercept_system_reset(caps.intercept_system_reset_available())
        .with_intercept_restore_partition_time(restore_partition_time_available);

    hcl.set_vtl2_vsm_partition_config(config)
        .map_err(Error::SetReg)
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
    /// Disable proxy interrupt redirection.
    pub disable_proxy_redirect: bool,
    /// Disable lower VTL timer virtualization.
    pub disable_lower_vtl_timer_virt: bool,
}

/// Parameters to [`UhProtoPartition::build`].
pub struct UhLateParams<'a> {
    /// Guest memory for lower VTLs.
    pub gm: VtlArray<GuestMemory, 2>,
    /// Guest memory for VTL 0 kernel execute access.
    pub vtl0_kernel_exec_gm: GuestMemory,
    /// Guest memory for VTL 0 user execute access.
    pub vtl0_user_exec_gm: GuestMemory,
    /// The CPUID leaves to expose to the guest.
    #[cfg(guest_arch = "x86_64")]
    pub cpuid: Vec<CpuidLeaf>,
    /// The mesh sender to use for crash notifications.
    // FUTURE: remove mesh dependency from this layer.
    pub crash_notification_send: mesh::Sender<VtlCrash>,
    /// The VM time source.
    pub vmtime: &'a VmTimeSource,
    /// Parameters for CVMs only.
    pub cvm_params: Option<CvmLateParams>,
    /// vmbus_relay is enabled and active for partition
    pub vmbus_relay: bool,
}

/// CVM-only parameters to [`UhProtoPartition::build`].
pub struct CvmLateParams {
    /// Guest memory for untrusted devices, like overlay pages.
    pub shared_gm: GuestMemory,
    /// An object to call to change host visibility on guest memory.
    pub isolated_memory_protector: Arc<dyn ProtectIsolatedMemory>,
    /// Dma client for shared visibility pages.
    pub shared_dma_client: Arc<dyn DmaClient>,
    /// Allocator for private visibility pages.
    pub private_dma_client: Arc<dyn DmaClient>,
}

/// Represents a GPN that is either in guest memory or was allocated by dma_client.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GpnSource {
    /// The GPN is in regular guest RAM.
    GuestMemory,
    /// The GPN was allocated by dma_client and is not in guest RAM.
    Dma,
}

/// Trait for CVM-related protections on guest memory.
pub trait ProtectIsolatedMemory: Send + Sync {
    /// Changes host visibility on guest memory.
    fn change_host_visibility(
        &self,
        vtl: GuestVtl,
        shared: bool,
        gpns: &[u64],
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), (HvError, usize)>;

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
        target_vtl: GuestVtl,
        protections: HvMapGpaFlags,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), HvError>;

    /// Changes the vtl protections on a range of guest memory.
    fn change_vtl_protections(
        &self,
        target_vtl: GuestVtl,
        gpns: &[u64],
        protections: HvMapGpaFlags,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), (HvError, usize)>;

    /// Registers a page as an overlay page by first validating it has the
    /// required permissions, optionally modifying them, then locking them.
    fn register_overlay_page(
        &self,
        vtl: GuestVtl,
        gpn: u64,
        gpn_source: GpnSource,
        check_perms: HvMapGpaFlags,
        new_perms: Option<HvMapGpaFlags>,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), HvError>;

    /// Unregisters an overlay page, removing its permission lock and restoring
    /// the previous permissions.
    fn unregister_overlay_page(
        &self,
        vtl: GuestVtl,
        gpn: u64,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), HvError>;

    /// Checks whether a page is currently registered as an overlay page.
    fn is_overlay_page(&self, vtl: GuestVtl, gpn: u64) -> bool;

    /// Locks the permissions and mappings for a set of guest pages.
    fn lock_gpns(&self, vtl: GuestVtl, gpns: &[u64]) -> Result<(), GuestMemoryBackingError>;

    /// Unlocks the permissions and mappings for a set of guest pages.
    ///
    /// Panics if asked to unlock a page that was not previously locked. The
    /// caller must ensure that the given slice has the same ordering as the
    /// one passed to `lock_gpns`.
    fn unlock_gpns(&self, vtl: GuestVtl, gpns: &[u64]);

    /// Alerts the memory protector that vtl 1 is ready to set vtl protections
    /// on lower-vtl memory, and that these protections should be enforced.
    fn set_vtl1_protections_enabled(&self);

    /// Whether VTL 1 is prepared to modify vtl protections on lower-vtl memory,
    /// and therefore whether these protections should be enforced.
    fn vtl1_protections_enabled(&self) -> bool;
}

/// Trait for access to TLB flush and lock machinery.
pub trait TlbFlushLockAccess {
    /// Flush the entire TLB for all VPs for the given VTL.
    fn flush(&mut self, vtl: GuestVtl);

    /// Flush the entire TLB for all VPs for all VTLs.
    fn flush_entire(&mut self);

    /// Causes the specified VTL on the current VP to wait on all TLB locks.
    fn set_wait_for_tlb_locks(&mut self, vtl: GuestVtl);
}

/// A partially built partition. Used to allow querying partition capabilities
/// before fully instantiating the partition.
pub struct UhProtoPartition<'a> {
    params: UhPartitionNewParams<'a>,
    hcl: Hcl,
    guest_vsm_available: bool,
    create_partition_available: bool,
    #[cfg(guest_arch = "x86_64")]
    cpuid: virt::CpuidLeafSet,
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

        set_vtl2_vsm_partition_config(&hcl)?;

        let privs = hcl
            .get_privileges_and_features_info()
            .map_err(Error::GetReg)?;
        let guest_vsm_available = Self::check_guest_vsm_support(privs, &hcl)?;

        #[cfg(guest_arch = "x86_64")]
        let cpuid = match params.isolation {
            IsolationType::Snp => cvm_cpuid::CpuidResultsIsolationType::Snp {
                cpuid_pages: params.cvm_cpuid_info.unwrap(),
                vtom: params.vtom.unwrap(),
                access_vsm: guest_vsm_available,
            }
            .build()
            .map_err(Error::CvmCpuid)?,

            IsolationType::Tdx => cvm_cpuid::CpuidResultsIsolationType::Tdx {
                topology: params.topology,
                vtom: params.vtom.unwrap(),
                access_vsm: guest_vsm_available,
            }
            .build()
            .map_err(Error::CvmCpuid)?,
            IsolationType::Vbs | IsolationType::None => Default::default(),
        };

        Ok(UhProtoPartition {
            hcl,
            params,
            guest_vsm_available,
            create_partition_available: privs.create_partitions(),
            #[cfg(guest_arch = "x86_64")]
            cpuid,
        })
    }

    /// Returns whether VSM support will be available to the guest.
    pub fn guest_vsm_available(&self) -> bool {
        self.guest_vsm_available
    }

    /// Returns whether this partition has the create partitions hypercall
    /// available.
    pub fn create_partition_available(&self) -> bool {
        self.create_partition_available
    }

    /// Returns a new Underhill partition.
    pub async fn build(
        self,
        late_params: UhLateParams<'_>,
    ) -> Result<(UhPartition, Vec<UhProcessorBox>), Error> {
        let Self {
            mut hcl,
            params,
            guest_vsm_available,
            create_partition_available: _,
            #[cfg(guest_arch = "x86_64")]
            cpuid,
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
                HvInterceptParameters::new_exception(
                    x86defs::Exception::SEV_VMM_COMMUNICATION.0 as u16,
                ),
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

        // Do per-VP HCL initialization.
        hcl.add_vps(
            params.topology.vp_count(),
            late_params
                .cvm_params
                .as_ref()
                .map(|x| &x.private_dma_client),
        )
        .map_err(Error::Hcl)?;

        let vps: Vec<_> = params
            .topology
            .vps_arch()
            .map(|vp_info| {
                // TODO: determine CPU index, which in theory could be different
                // from the VP index, though this hasn't happened yet.
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
        let caps = virt::aarch64::Aarch64PartitionCapabilities {};

        #[cfg(guest_arch = "x86_64")]
        let cpuid = UhPartition::construct_cpuid_results(
            cpuid,
            &late_params.cpuid,
            params.topology,
            isolation,
            params.hide_isolation,
        );

        #[cfg(guest_arch = "x86_64")]
        let caps = UhPartition::construct_capabilities(
            params.topology,
            &cpuid,
            isolation,
            params.hide_isolation,
        )
        .map_err(Error::Capabilities)?;

        if params.handle_synic && !matches!(isolation, IsolationType::Tdx) {
            // The hypervisor will manage the untrusted SINTs (or the whole
            // synic for non-hardware-isolated VMs), but some event ports
            // and message ports are implemented here. Register an intercept
            // to handle HvSignalEvent and HvPostMessage hypercalls when the
            // hypervisor doesn't recognize the connection ID.
            //
            // TDX manages this locally instead of through the hypervisor.
            hcl.register_intercept(
                HvInterceptType::HvInterceptTypeUnknownSynicConnection,
                HV_INTERCEPT_ACCESS_MASK_EXECUTE,
                HvInterceptParameters::new_zeroed(),
            )
            .expect("registering synic intercept cannot fail");
        }

        #[cfg(guest_arch = "x86_64")]
        let cvm_state = if is_hardware_isolated {
            let vsm_caps = hcl.get_vsm_capabilities().map_err(Error::GetReg)?;
            let proxy_interrupt_redirect_available =
                vsm_caps.proxy_interrupt_redirect_available() && !params.disable_proxy_redirect;

            Some(Self::construct_cvm_state(
                &params,
                late_params.cvm_params.unwrap(),
                &caps,
                guest_vsm_available,
                proxy_interrupt_redirect_available,
            )?)
        } else {
            None
        };
        #[cfg(guest_arch = "aarch64")]
        let cvm_state = None;

        let lower_vtl_timer_virt_available =
            hcl.supports_lower_vtl_timer_virt() && !params.disable_lower_vtl_timer_virt;

        let backing_shared = BackingShared::new(
            isolation,
            &params,
            BackingSharedParams {
                cvm_state,
                #[cfg(guest_arch = "x86_64")]
                cpuid: &cpuid,
                hcl: &hcl,
                guest_vsm_available,
                lower_vtl_timer_virt_available,
            },
        )?;

        let enter_modes = EnterModes::default();

        let partition = Arc::new(UhPartitionInner {
            hcl,
            vps,
            irq_routes: Default::default(),
            caps,
            enter_modes: Mutex::new(enter_modes),
            enter_modes_atomic: u8::from(hcl::protocol::EnterModes::from(enter_modes)).into(),
            gm: late_params.gm,
            vtl0_kernel_exec_gm: late_params.vtl0_kernel_exec_gm,
            vtl0_user_exec_gm: late_params.vtl0_user_exec_gm,
            #[cfg(guest_arch = "x86_64")]
            cpuid,
            crash_notification_send: late_params.crash_notification_send,
            monitor_page: MonitorPage::new(),
            allocated_monitor_page: Mutex::new(None),
            software_devices,
            lower_vtl_memory_layout: params.lower_vtl_memory_layout.clone(),
            vmtime: late_params.vmtime.clone(),
            isolation,
            no_sidecar_hotplug: params.no_sidecar_hotplug.into(),
            use_mmio_hypercalls: params.use_mmio_hypercalls,
            backing_shared,
            #[cfg(guest_arch = "x86_64")]
            device_vector_table: RwLock::new(IrrBitmap::new(Default::default())),
            intercept_debug_exceptions: params.intercept_debug_exceptions,
            vmbus_relay: late_params.vmbus_relay,
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
    pub fn vtl0_guest_os_id(&self) -> Result<HvGuestOsId, hcl::ioctl::register::GetRegError> {
        // If Underhill is emulating the hypervisor interfaces, get this value
        // from the emulator. This happens when running under hardware isolation
        // or when configured for testing.
        let id = if let Some(hv) = self.inner.hv() {
            hv.guest_os_id(Vtl::Vtl0)
        } else {
            // Ask the hypervisor for this value.
            self.inner.hcl.get_guest_os_id(GuestVtl::Vtl0)?
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

    /// Trigger the LINT1 interrupt vector on the LAPIC of the BSP.
    pub fn assert_debug_interrupt(&self, _vtl: u8) {
        #[cfg(guest_arch = "x86_64")]
        const LINT_INDEX_1: u8 = 1;
        #[cfg(guest_arch = "x86_64")]
        match self.inner.isolation {
            IsolationType::Snp => {
                tracing::error!(?_vtl, "Debug interrupts cannot be injected into SNP VMs",);
            }
            _ => {
                let bsp_index = VpIndex::new(0);
                self.pulse_lint(bsp_index, Vtl::try_from(_vtl).unwrap(), LINT_INDEX_1)
            }
        }
    }

    /// Enables or disables the PM timer assist.
    pub fn set_pm_timer_assist(
        &self,
        port: Option<u16>,
    ) -> Result<(), hcl::ioctl::register::SetRegError> {
        self.inner.hcl.set_pm_timer_assist(port)
    }

    /// Sets guest memory protections for a monitor page.
    fn register_cvm_dma_overlay_page(
        &self,
        vtl: GuestVtl,
        gpn: u64,
        new_perms: HvMapGpaFlags,
    ) -> anyhow::Result<()> {
        // How the monitor page is protected depends on the isolation type of the VM.
        match &self.inner.backing_shared {
            #[cfg(guest_arch = "x86_64")]
            BackingShared::Snp(snp_backed_shared) => snp_backed_shared
                .cvm
                .isolated_memory_protector
                .register_overlay_page(
                    vtl,
                    gpn,
                    // On a CVM, the monitor page is always DMA-allocated.
                    GpnSource::Dma,
                    HvMapGpaFlags::new(),
                    Some(new_perms),
                    &mut SnpBacked::tlb_flush_lock_access(
                        None,
                        self.inner.as_ref(),
                        snp_backed_shared,
                    ),
                )
                .map_err(|e| anyhow::anyhow!(e)),
            #[cfg(guest_arch = "x86_64")]
            BackingShared::Tdx(tdx_backed_shared) => tdx_backed_shared
                .cvm
                .isolated_memory_protector
                .register_overlay_page(
                    vtl,
                    gpn,
                    GpnSource::Dma,
                    HvMapGpaFlags::new(),
                    Some(new_perms),
                    &mut TdxBacked::tlb_flush_lock_access(
                        None,
                        self.inner.as_ref(),
                        tdx_backed_shared,
                    ),
                )
                .map_err(|e| anyhow::anyhow!(e)),
            BackingShared::Hypervisor(_) => {
                let _ = (vtl, gpn, new_perms);
                unreachable!()
            }
        }
    }

    /// Reverts guest memory protections for a monitor page.
    fn unregister_cvm_dma_overlay_page(&self, vtl: GuestVtl, gpn: u64) -> anyhow::Result<()> {
        // How the monitor page is protected depends on the isolation type of the VM.
        match &self.inner.backing_shared {
            #[cfg(guest_arch = "x86_64")]
            BackingShared::Snp(snp_backed_shared) => snp_backed_shared
                .cvm
                .isolated_memory_protector
                .unregister_overlay_page(
                    vtl,
                    gpn,
                    &mut SnpBacked::tlb_flush_lock_access(
                        None,
                        self.inner.as_ref(),
                        snp_backed_shared,
                    ),
                )
                .map_err(|e| anyhow::anyhow!(e)),
            #[cfg(guest_arch = "x86_64")]
            BackingShared::Tdx(tdx_backed_shared) => tdx_backed_shared
                .cvm
                .isolated_memory_protector
                .unregister_overlay_page(
                    vtl,
                    gpn,
                    &mut TdxBacked::tlb_flush_lock_access(
                        None,
                        self.inner.as_ref(),
                        tdx_backed_shared,
                    ),
                )
                .map_err(|e| anyhow::anyhow!(e)),
            BackingShared::Hypervisor(_) => {
                let _ = (vtl, gpn);
                unreachable!()
            }
        }
    }
}

impl UhProtoPartition<'_> {
    /// Whether Guest VSM is available to the guest. If so, for hardware CVMs,
    /// it is safe to expose Guest VSM support via cpuid.
    fn check_guest_vsm_support(privs: HvPartitionPrivilege, hcl: &Hcl) -> Result<bool, Error> {
        if !privs.access_vsm() {
            return Ok(false);
        }

        let guest_vsm_config = hcl
            .get_guest_vsm_partition_config()
            .map_err(Error::GetReg)?;
        Ok(guest_vsm_config.maximum_vtl() >= u8::from(GuestVtl::Vtl1))
    }

    #[cfg(guest_arch = "x86_64")]
    /// Constructs partition-wide CVM state.
    fn construct_cvm_state(
        params: &UhPartitionNewParams<'_>,
        late_params: CvmLateParams,
        caps: &PartitionCapabilities,
        guest_vsm_available: bool,
        proxy_interrupt_redirect_available: bool,
    ) -> Result<UhCvmPartitionState, Error> {
        use vmcore::reference_time::ReferenceTimeSource;

        let vp_count = params.topology.vp_count() as usize;
        let vps = (0..vp_count)
            .map(|vp_index| UhCvmVpInner {
                tlb_lock_info: VtlArray::from_fn(|_| TlbLockInfo::new(vp_count)),
                vtl1_enable_called: Mutex::new(false),
                started: AtomicBool::new(vp_index == 0),
                hv_start_enable_vtl_vp: VtlArray::from_fn(|_| Mutex::new(None)),
                proxy_redirect_interrupts: Mutex::new(HashMap::new()),
            })
            .collect();
        let tlb_locked_vps =
            VtlArray::from_fn(|_| BitVec::repeat(false, vp_count).into_boxed_bitslice());

        let lapic = VtlArray::from_fn(|_| {
            LocalApicSet::builder()
                .x2apic_capable(caps.x2apic)
                .hyperv_enlightenments(true)
                .build()
        });

        let tsc_frequency = get_tsc_frequency(params.isolation)?;
        let ref_time = ReferenceTimeSource::new(TscReferenceTimeSource::new(tsc_frequency));

        // If we're emulating the APIC, then we also must emulate the hypervisor
        // enlightenments, since the hypervisor can't support enlightenments
        // without also providing an APIC.
        //
        // Additionally, TDX provides hardware APIC emulation but we still need
        // to emulate the hypervisor enlightenments.
        let hv = GlobalHv::new(hv1_emulator::hv::GlobalHvParams {
            max_vp_count: params.topology.vp_count(),
            vendor: caps.vendor,
            tsc_frequency,
            ref_time,
            is_ref_time_backed_by_tsc: true,
        });

        Ok(UhCvmPartitionState {
            vps_per_socket: params.topology.reserved_vps_per_socket(),
            tlb_locked_vps,
            vps,
            shared_memory: late_params.shared_gm,
            isolated_memory_protector: late_params.isolated_memory_protector,
            lapic,
            hv,
            guest_vsm: RwLock::new(GuestVsmState::from_availability(guest_vsm_available)),
            shared_dma_client: late_params.shared_dma_client,
            private_dma_client: late_params.private_dma_client,
            hide_isolation: params.hide_isolation,
            proxy_interrupt_redirect: proxy_interrupt_redirect_available,
        })
    }
}

impl UhPartition {
    #[cfg(guest_arch = "x86_64")]
    /// Constructs the set of cpuid results to show to the guest
    fn construct_cpuid_results(
        cpuid: virt::CpuidLeafSet,
        initial_cpuid: &[CpuidLeaf],
        topology: &ProcessorTopology<vm_topology::processor::x86::X86Topology>,
        isolation: IsolationType,
        hide_isolation: bool,
    ) -> virt::CpuidLeafSet {
        let mut cpuid = cpuid.into_leaves();
        if isolation.is_hardware_isolated() {
            // Update the x2apic leaf based on the topology.
            let x2apic = match topology.apic_mode() {
                vm_topology::processor::x86::ApicMode::XApic => false,
                vm_topology::processor::x86::ApicMode::X2ApicSupported => true,
                vm_topology::processor::x86::ApicMode::X2ApicEnabled => true,
            };
            let ecx = x86defs::cpuid::VersionAndFeaturesEcx::new().with_x2_apic(x2apic);
            let ecx_mask = x86defs::cpuid::VersionAndFeaturesEcx::new().with_x2_apic(true);
            cpuid.push(
                CpuidLeaf::new(
                    x86defs::cpuid::CpuidFunction::VersionAndFeatures.0,
                    [0, 0, ecx.into(), 0],
                )
                .masked([0, 0, ecx_mask.into(), 0]),
            );

            // Get the hypervisor version from the host. This is just for
            // reporting purposes, so it is safe even if the hypervisor is not
            // trusted.
            let hv_version = safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_VERSION, 0);

            // Perform final processing steps for synthetic leaves.
            hv1_emulator::cpuid::process_hv_cpuid_leaves(
                &mut cpuid,
                hide_isolation,
                [
                    hv_version.eax,
                    hv_version.ebx,
                    hv_version.ecx,
                    hv_version.edx,
                ],
            );
        }
        cpuid.extend(initial_cpuid);
        virt::CpuidLeafSet::new(cpuid)
    }

    #[cfg(guest_arch = "x86_64")]
    /// Computes the partition capabilities
    fn construct_capabilities(
        topology: &ProcessorTopology,
        cpuid: &virt::CpuidLeafSet,
        isolation: IsolationType,
        hide_isolation: bool,
    ) -> Result<virt::x86::X86PartitionCapabilities, virt::x86::X86PartitionCapabilitiesError> {
        let mut native_cpuid_fn;
        let mut cvm_cpuid_fn;

        // Determine the method to get cpuid results for the guest when
        // computing partition capabilities.
        let cpuid_fn: &mut dyn FnMut(u32, u32) -> [u32; 4] = if isolation.is_hardware_isolated() {
            // Use the filtered CPUID to determine capabilities.
            cvm_cpuid_fn = move |leaf, sub_leaf| cpuid.result(leaf, sub_leaf, &[0, 0, 0, 0]);
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
        let mut caps = virt::x86::X86PartitionCapabilities::from_cpuid(topology, cpuid_fn)?;
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

        Ok(caps)
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
                    tracing::warn!(
                        CVM_ALLOWED,
                        "old hypervisor build; using slow path for intercept ranges"
                    );
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
#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
fn validate_vtl_gpa_flags(
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
