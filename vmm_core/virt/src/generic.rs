// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod partition_memory_map;

pub use partition_memory_map::PartitionMemoryMap;
use pci_core::msi::SignalMsi;
pub use vm_topology::processor::VpIndex;

use crate::CpuidLeaf;
use crate::PartitionCapabilities;
use crate::io::CpuIo;
use crate::irqcon::ControlGic;
use crate::irqcon::IoApicRouting;
use crate::irqcon::MsiRequest;
use crate::x86::DebugState;
use crate::x86::HardwareBreakpoint;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use hvdef::Vtl;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use std::cell::Cell;
use std::convert::Infallible;
use std::fmt::Debug;
use std::future::Future;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::task::Waker;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vmcore::monitor::MonitorId;
use vmcore::reference_time::ReferenceTimeSource;
use vmcore::synic::GuestEventPort;
use vmcore::vmtime::VmTimeSource;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vmcore::vpci_msi::VpciInterruptParameters;

pub trait Hypervisor: 'static {
    /// The prototype partition type.
    type ProtoPartition<'a>: ProtoPartition<Partition = Self::Partition>;
    /// The partition type.
    type Partition;
    /// The error type when creating the partition.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Returns whether this hypervisor is available on this machine.
    fn is_available(&self) -> Result<bool, Self::Error>;

    /// Returns a new prototype partition from the given configuration.
    fn new_partition<'a>(
        &'a mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<Self::ProtoPartition<'a>, Self::Error>;
}

/// Isolation type for a partition.
#[derive(Eq, PartialEq, Debug, Copy, Clone, Inspect)]
pub enum IsolationType {
    /// No isolation.
    None,
    /// Hypervisor based isolation.
    Vbs,
    /// Secure nested paging (AMD SEV-SNP) - hardware based isolation.
    Snp,
    /// Trust domain extensions (Intel TDX) - hardware based isolation.
    Tdx,
}

impl IsolationType {
    /// Returns true if the isolation type is not `None`.
    pub fn is_isolated(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Returns whether the isolation type is hardware-backed.
    pub fn is_hardware_isolated(&self) -> bool {
        matches!(self, Self::Snp | Self::Tdx)
    }
}

/// An unexpected isolation type was provided.
#[derive(Debug)]
pub struct UnexpectedIsolationType;

impl IsolationType {
    pub const fn from_hv(
        value: hvdef::HvPartitionIsolationType,
    ) -> Result<Self, UnexpectedIsolationType> {
        match value {
            hvdef::HvPartitionIsolationType::NONE => Ok(IsolationType::None),
            hvdef::HvPartitionIsolationType::VBS => Ok(IsolationType::Vbs),
            hvdef::HvPartitionIsolationType::SNP => Ok(IsolationType::Snp),
            hvdef::HvPartitionIsolationType::TDX => Ok(IsolationType::Tdx),
            _ => Err(UnexpectedIsolationType),
        }
    }

    pub const fn to_hv(self) -> hvdef::HvPartitionIsolationType {
        match self {
            IsolationType::None => hvdef::HvPartitionIsolationType::NONE,
            IsolationType::Vbs => hvdef::HvPartitionIsolationType::VBS,
            IsolationType::Snp => hvdef::HvPartitionIsolationType::SNP,
            IsolationType::Tdx => hvdef::HvPartitionIsolationType::TDX,
        }
    }
}

/// Page visibility types for isolated partitions.
#[derive(Eq, PartialEq, Debug, Copy, Clone, Inspect)]
pub enum PageVisibility {
    /// The guest has exclusive access to the page, and no access from the host.
    Exclusive,
    /// The page has shared access with the guest and host.
    Shared,
}

/// Prototype partition creation configuration.
pub struct ProtoPartitionConfig<'a> {
    /// The set of VPs to create.
    pub processor_topology: &'a ProcessorTopology,
    /// Microsoft hypervisor guest interface configuration.
    pub hv_config: Option<HvConfig>,
    /// VM time access.
    pub vmtime: &'a VmTimeSource,
    /// Use the user-mode APIC emulator, if supported.
    pub user_mode_apic: bool,
    /// Isolation type for this partition.
    pub isolation: IsolationType,
}

/// Partition creation configuration.
pub struct PartitionConfig<'a> {
    /// The guest memory layout.
    pub mem_layout: &'a MemoryLayout,
    /// Guest memory access.
    pub guest_memory: &'a GuestMemory,
    /// Cpuid leaves to add to the default CPUID results.
    pub cpuid: &'a [CpuidLeaf],
    /// The offset of the VTL0 alias map. This maps VTL0's view of memory into
    /// VTL2 at the specified offset (which must be a power of 2).
    pub vtl0_alias_map: Option<u64>,
}

/// Trait for a prototype partition, one that is partially created but still
/// needs final configuration.
///
/// This is separate from the partition so that it can be queried to determine
/// the final partition configuration.
pub trait ProtoPartition {
    /// The partition type.
    type Partition: Partition;
    /// The VP binder type.
    type ProcessorBinder: 'static + BindProcessor + Send;
    /// The error type when creating the partition.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Gets the default guest cpuid value for inputs `eax` and `ecx`.
    #[cfg(guest_arch = "x86_64")]
    fn cpuid(&self, eax: u32, ecx: u32) -> [u32; 4];

    /// The maximum physical address width that processors and devices for this
    /// partition can access.
    ///
    /// This may be smaller than what is reported to the guest via architectural
    /// interfaces by default, and it may be larger or smaller than what the VMM
    /// ultimately chooses to report to the guest.
    fn max_physical_address_size(&self) -> u8;

    /// Constructs the full partition.
    fn build(
        self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error>;
}

/// Trait used to bind a processor to the current thread.
pub trait BindProcessor {
    /// The processor object.
    type Processor<'a>: Processor
    where
        Self: 'a;

    /// A binding error.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Binds the processor to the current thread.
    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error>;
}

/// Policy for the partition when mapping VTL0 memory late.
#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum LateMapVtl0MemoryPolicy {
    /// Halt execution of the VP if VTL0 memory is accessed.
    Halt,
    /// Log the error but emulate the access with the instruction emulator.
    Log,
    /// Inject an exception into the guest.
    InjectException,
}

/// Which ranges VTL2 is allowed to access before VTL0 ram is mapped.
#[derive(Debug, Clone)]
pub enum LateMapVtl0AllowedRanges {
    /// Ask the memory layout what the vtl2_ram ranges are.
    MemoryLayout,
    /// These specific ranges are allowed.
    Ranges(Vec<MemoryRange>),
}

/// Config used to determine late mapping VTL0 memory.
#[derive(Debug, Clone)]
pub struct LateMapVtl0MemoryConfig {
    /// What ranges VTL2 are allowed to access before VTL0 memory is mapped.
    /// Generally this consists of the ranges representing VTL2 ram.
    pub allowed_ranges: LateMapVtl0AllowedRanges,
    /// The policy for the partition mapping VTL0 memory late.
    pub policy: LateMapVtl0MemoryPolicy,
}

/// VTL2 configuration.
#[derive(Debug)]
pub struct Vtl2Config {
    /// If set, map VTL0 memory late after VTL2 has started. The current
    /// heuristic is to defer mapping VTL0 memory until the first
    /// [`hvdef::HypercallCode::HvCallModifyVtlProtectionMask`] hypercall is
    /// made.
    ///
    /// Accesses before memory is mapped is determined by the specified config.
    pub late_map_vtl0_memory: Option<LateMapVtl0MemoryConfig>,
}

/// Hypervisor configuration.
#[derive(Debug)]
pub struct HvConfig {
    /// Use the hypervisor's in-built enlightenment support if available.
    pub offload_enlightenments: bool,
    /// Allow device assignment on the partition.
    pub allow_device_assignment: bool,
    /// Enable VTL2 support if set. Additional options are described by
    /// [Vtl2Config].
    pub vtl2: Option<Vtl2Config>,
}

/// Methods for manipulating a VM partition.
pub trait Partition: 'static + Hv1 + Inspect + Send + Sync {
    /// Returns a trait object to accept pages on behalf of the guest during the
    /// initial start import flow.
    fn supports_initial_accept_pages(
        &self,
    ) -> Option<&dyn AcceptInitialPages<Error = <Self as Hv1>::Error>> {
        None
    }

    /// Returns a trait object to reset the partition, if supported.
    fn supports_reset(&self) -> Option<&dyn ResetPartition<Error = <Self as Hv1>::Error>>;

    /// Returns a trait object to reset VTL state, if supported.
    fn supports_vtl_scrub(&self) -> Option<&dyn ScrubVtl<Error = <Self as Hv1>::Error>> {
        None
    }

    /// Returns an interface for registering MMIO doorbells for this partition.
    ///
    /// Not all partitions support this.
    fn doorbell_registration(
        self: &Arc<Self>,
        minimum_vtl: Vtl,
    ) -> Option<Arc<dyn DoorbellRegistration>> {
        let _ = minimum_vtl;
        None
    }

    /// Requests an MSI for the specified VTL.
    ///
    /// On x86, the MSI format is the architectural APIC format.
    ///
    /// On ARM64, the MSI format is currently not defined, since we only support
    /// Hyper-V-style VMs (which use synthetic MSIs via VPCI). In the future, we
    /// may want to support either or both SPI- and ITS+LPI-based MSIs.
    fn request_msi(&self, vtl: Vtl, request: MsiRequest);

    /// Returns an MSI interrupt target for this partition, which can be used to
    /// create MSI interrupts.
    ///
    /// Not all partitions support this.
    fn as_signal_msi(self: &Arc<Self>, vtl: Vtl) -> Option<Arc<dyn SignalMsi>> {
        let _ = vtl;
        None
    }

    /// Get the partition capabilities for this partition.
    fn caps(&self) -> &PartitionCapabilities;

    /// Forces the run_vp call to yield to the scheduler (i.e. return
    /// Poll::Pending).
    fn request_yield(&self, vp_index: VpIndex);
}

/// X86-specific partition methods.
pub trait X86Partition: Partition {
    /// Gets the IO-APIC routing control for VTL0.
    fn ioapic_routing(&self) -> Arc<dyn IoApicRouting>;

    /// Pulses the specified APIC's local interrupt line (0 or 1).
    fn pulse_lint(&self, vp_index: VpIndex, vtl: Vtl, lint: u8);
}

/// ARM64-specific partition methods.
pub trait Aarch64Partition: Partition {
    /// Returns an interface for accessing the GIC interrupt controller for `vtl`.
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn ControlGic>;
}

/// Extension trait for accepting initial pages.
pub trait AcceptInitialPages {
    type Error: std::error::Error;

    /// Accepts initial pages on behalf of the guest.
    ///
    /// This can only be used during the load path during partition start to
    /// accept pages on behalf of the guest that were set as part of the load
    /// process. The host virtstack cannot accept pages on behalf of the guest
    /// once it has started running.
    fn accept_initial_pages(
        &self,
        pages: &[(MemoryRange, PageVisibility)],
    ) -> Result<(), Self::Error>;
}

/// Extension trait for resetting the partition.
pub trait ResetPartition {
    type Error: std::error::Error;

    /// Resets the partition, restoring all partition state to the initial
    /// state.
    ///
    /// The caller must ensure that no VPs are running when this is called.
    ///
    /// If this fails, the partition is in a bad state and cannot be resumed
    /// until a subsequent reset call succeeds.
    fn reset(&self) -> Result<(), Self::Error>;
}

/// Extension trait for scrubbing higher VTL state while leaving lower VTLs
/// untouched.
pub trait ScrubVtl {
    type Error: std::error::Error;

    /// Scrubs partition and VP state for `vtl`. This is useful for servicing
    /// and restarting a higher VTL without touching the lower VTL.
    ///
    /// The caller must ensure that no VPs are running when this is called.
    ///
    /// Note that this does not reset page protections. This is necessary
    /// because there may be devices assigned to lower VTLs, and they should not
    /// be able to DMA to higher VTL memory during servicing.
    fn scrub(&self, vtl: Vtl) -> Result<(), Self::Error>;
}

/// Provides access to partition state for save, restore, and reset.
///
/// This is not part of [`Partition`] because some scenarios do not require such
/// access.
pub trait PartitionAccessState {
    type StateAccess<'a>: crate::vm::AccessVmState
    where
        Self: 'a;

    /// Returns an object to access VM state for the specified VTL.
    fn access_state(&self, vtl: Vtl) -> Self::StateAccess<'_>;
}

/// Change memory protections for lower VTLs. This can be used to share memory
/// with a lower VTL or make memory accesses trigger an intercept. This is
/// intended for dynamic state as initial memory protections are applied at VM
/// start.
pub trait VtlMemoryProtection {
    /// Sets lower VTL permissions on a physical page.
    ///
    /// TODO: To remain generic may want to replace hvdef::HvMapGpaFlags with
    ///       something else.
    fn modify_vtl_page_setting(&self, pfn: u64, flags: hvdef::HvMapGpaFlags) -> anyhow::Result<()>;
}

pub trait Processor: InspectMut {
    type StateAccess<'a>: crate::vp::AccessVpState
    where
        Self: 'a;

    /// Sets the debug state: conditions under which the VP should exit for
    /// debugging the guest. This including single stepping and hardware
    /// breakpoints.
    ///
    /// TODO: generalize for non-x86 architectures.
    fn set_debug_state(
        &mut self,
        vtl: Vtl,
        state: Option<&DebugState>,
    ) -> Result<(), <Self::StateAccess<'_> as crate::vp::AccessVpState>::Error>;

    /// Runs the VP.
    ///
    /// Although this is an async function, it may block synchronously until
    /// [`Partition::request_yield`] is called for this VP. Then its future must
    /// return [`Poll::Pending`] at least once.
    ///
    /// Returns when an error occurs, the VP halts, or the VP is requested to
    /// stop via `stop`.
    #[expect(async_fn_in_trait)] // don't need or want Send bound
    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason>;

    /// Without running the VP, flushes any asynchronous requests from other
    /// processors or objects that might affect this state, so that the object
    /// can be saved/restored correctly.
    fn flush_async_requests(&mut self);

    /// Returns whether the specified VTL can be inspected on this processor.
    ///
    /// VTL0 is always inspectable.
    fn vtl_inspectable(&self, vtl: Vtl) -> bool {
        vtl == Vtl::Vtl0
    }

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_>;
}

/// A source for [`StopVp`].
pub struct StopVpSource {
    stop: Cell<bool>,
    waker: Cell<Option<Waker>>,
}

impl StopVpSource {
    /// Creates a new source.
    pub fn new() -> Self {
        Self {
            stop: Cell::new(false),
            waker: Cell::new(None),
        }
    }

    /// Returns an object to wait for stops.
    pub fn checker(&self) -> StopVp<'_> {
        StopVp { source: self }
    }

    /// Initiates a VP stop.
    ///
    /// After this, calls to [`StopVp::check`] or [`StopVp::until_stop`] will
    /// fail.
    pub fn stop(&self) {
        self.stop.set(true);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    /// Returns whether [`Self::stop`] has been called.
    pub fn is_stopping(&self) -> bool {
        self.stop.get()
    }
}

/// Object to check for VP stop requests.
pub struct StopVp<'a> {
    source: &'a StopVpSource,
}

/// An error result that the VP stopped due to request.
#[derive(Debug)]
pub struct VpStopped(());

impl StopVp<'_> {
    /// Returns `Err(VpStopped(_))` if the VP should stop.
    pub fn check(&self) -> Result<(), VpStopped> {
        if self.source.stop.get() {
            Err(VpStopped(()))
        } else {
            Ok(())
        }
    }

    /// Runs `fut` until it completes or the VP should stop.
    pub async fn until_stop<Fut: Future>(&mut self, fut: Fut) -> Result<Fut::Output, VpStopped> {
        let mut fut = pin!(fut);
        poll_fn(|cx| match fut.as_mut().poll(cx) {
            Poll::Ready(r) => Poll::Ready(Ok(r)),
            Poll::Pending => {
                self.check()?;
                self.source.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            }
        })
        .await
    }
}

/// An object that can be polled to see if a yield has been requested.
#[derive(Debug)]
pub struct NeedsYield {
    yield_requested: AtomicBool,
}

impl NeedsYield {
    /// Creates a new object.
    pub fn new() -> Self {
        Self {
            yield_requested: false.into(),
        }
    }

    /// Requests a yield.
    ///
    /// Returns whether a signal is necessary to ensure that the task yields
    /// soon.
    pub fn request_yield(&self) -> bool {
        !self.yield_requested.swap(true, Ordering::Release)
    }

    /// Yields execution to the executor if `request_yield` has been called
    /// since the last call to `maybe_yield`.
    pub async fn maybe_yield(&self) {
        poll_fn(|cx| {
            if self.yield_requested.load(Ordering::Acquire) {
                // Wake this task again to ensure it runs again.
                cx.waker().wake_by_ref();
                self.yield_requested.store(false, Ordering::Relaxed);
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await
    }
}

/// The reason that [`Processor::run_vp`] returned.
#[derive(Debug)]
pub enum VpHaltReason {
    /// The processor was requested to stop.
    Stop(VpStopped),
    /// The processor task should be restarted, possibly on a different thread.
    Cancel,
    /// The processor initiated a power off.
    PowerOff,
    /// The processor initiated a reboot.
    Reset,
    /// The processor initiated a hibernation.
    Hibernate,
    /// The processor triple faulted.
    TripleFault {
        /// The faulting VTL.
        // FUTURE: move VTL state into `AccessVpState``.
        vtl: Vtl,
    },
    /// Debugger single step.
    SingleStep,
    /// Debugger hardware breakpoint.
    HwBreak(HardwareBreakpoint),
}

impl From<VpStopped> for VpHaltReason {
    fn from(stop: VpStopped) -> Self {
        Self::Stop(stop)
    }
}

pub trait PartitionMemoryMapper {
    /// Returns a memory mapper for the partition backing `vtl`.
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn PartitionMemoryMap>;
}

pub trait Hv1 {
    type Error: std::error::Error + Send + Sync + 'static;
    type Device: MapVpciInterrupt + SignalMsi;

    fn reference_time_source(&self) -> Option<ReferenceTimeSource>;

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn DeviceBuilder<Device = Self::Device, Error = Self::Error>>;
}

pub trait DeviceBuilder: Hv1 {
    fn build(&self, vtl: Vtl, device_id: u64) -> Result<Self::Device, Self::Error>;
}

pub enum UnimplementedDevice {}

impl MapVpciInterrupt for UnimplementedDevice {
    async fn register_interrupt(
        &self,
        _vector_count: u32,
        _params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        match *self {}
    }

    async fn unregister_interrupt(&self, _address: u64, _data: u32) {
        match *self {}
    }
}

impl SignalMsi for UnimplementedDevice {
    fn signal_msi(&self, _rid: u32, _address: u64, _data: u32) {
        match *self {}
    }
}

pub trait Synic: Send + Sync {
    /// Adds a fast path to signal `event` when the guest signals
    /// `connection_id` from VTL >= `minimum_vtl`.
    ///
    /// Returns Ok(None) if this acceleration is not supported.
    fn new_host_event_port(
        &self,
        connection_id: u32,
        minimum_vtl: Vtl,
        event: &pal_event::Event,
    ) -> Result<Option<Box<dyn Sync + Send>>, vmcore::synic::Error> {
        let _ = (connection_id, minimum_vtl, event);
        Ok(None)
    }

    /// Posts a message to the guest.
    fn post_message(&self, vtl: Vtl, vp: VpIndex, sint: u8, typ: u32, payload: &[u8]);

    /// Creates a [`GuestEventPort`] for signaling VMBus channels in the guest.
    fn new_guest_event_port(
        &self,
        vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort>;

    /// Returns whether callers should pass an OS event when creating event
    /// ports, as opposed to passing a function to call.
    ///
    /// This is true when the hypervisor can more quickly dispatch an OS event
    /// and resume the VP than it can take an intercept into user mode and call
    /// a function.
    fn prefer_os_events(&self) -> bool;

    /// Returns an object for manipulating the monitor page, or None if monitor pages aren't
    /// supported.
    fn monitor_support(&self) -> Option<&dyn SynicMonitor> {
        None
    }
}

/// Provides monitor page functionality for a `Synic` implementation.
pub trait SynicMonitor: Synic {
    /// Registers a monitored interrupt. The returned struct will unregister the ID when dropped.
    ///
    /// # Panics
    ///
    /// Panics if monitor_id is already in use.
    fn register_monitor(&self, monitor_id: MonitorId, connection_id: u32) -> Box<dyn Sync + Send>;

    /// Sets the GPA of the monitor page currently in use.
    fn set_monitor_page(&self, vtl: Vtl, gpa: Option<u64>) -> anyhow::Result<()>;

    /// Allocates a monitor page and sets it as the monitor page currently in use. If allocating
    /// monitor pages is not supported, returns `Ok(None)`.
    ///
    /// The page will be deallocated if the monitor page is subsequently changed or cleared using
    /// [`SynicMonitor::set_monitor_page`].
    fn allocate_monitor_page(&self, vtl: Vtl) -> anyhow::Result<Option<u64>> {
        let _ = vtl;
        Ok(None)
    }
}

/// MNF support routines for the emulator
pub trait EmulatorMonitorSupport {
    /// Check if the specified write is inside the monitor page, and signal the associated
    /// connection ID if it is.
    #[must_use]
    fn check_write(&self, gpa: u64, bytes: &[u8]) -> bool;

    /// Check if the specified read is inside the monitor page, and fill the provided buffer
    /// if it is.
    #[must_use]
    fn check_read(&self, gpa: u64, bytes: &mut [u8]) -> bool;
}
