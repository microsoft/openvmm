// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux /dev/mshv implementation of the virt::generic interfaces.

#![cfg(all(target_os = "linux", guest_is_native))]
// UNSAFETY: Calling HV APIs and manually managing memory.
#![expect(unsafe_code)]

#[cfg(guest_arch = "aarch64")]
mod aarch64;
#[cfg(guest_arch = "x86_64")]
mod x86_64;

#[cfg(guest_arch = "aarch64")]
use aarch64 as arch;
#[cfg(guest_arch = "x86_64")]
use x86_64 as arch;

// irqfd is arch-independent but only wired up on x86_64 for now.
// TODO: wire up on aarch64 once MSI signaling is implemented.
#[cfg(guest_arch = "x86_64")]
pub mod irqfd;

use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use hv1_emulator::message_queues::MessageQueues;
use hvdef::HV_PAGE_SHIFT;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::HvPartitionPropertyCode;
use hvdef::Vtl;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_EXECUTE;
use hvdef::hypercall::HvRegisterAssoc;
use inspect::Inspect;
use inspect::InspectMut;
use mshv_bindings::MSHV_SET_MEM_BIT_EXECUTABLE;
use mshv_bindings::MSHV_SET_MEM_BIT_WRITABLE;
use mshv_bindings::mshv_install_intercept;
use mshv_bindings::mshv_user_mem_region;
use mshv_ioctls::Mshv;
use mshv_ioctls::MshvError;
use mshv_ioctls::VcpuFd;
use mshv_ioctls::VmFd;
use mshv_ioctls::set_bits;
use pal::unix::pthread::*;
use pal_event::Event;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::convert::Infallible;
use std::future::poll_fn;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::IntoRawFd as _;
use std::sync::Arc;
use std::sync::Once;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Waker;
use thiserror::Error;
use virt::NeedsYield;
use virt::PartitionAccessState;
use virt::ProtoPartitionConfig;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use vmcore::interrupt::Interrupt;
use vmcore::reference_time::GetReferenceTime;
use vmcore::reference_time::ReferenceTimeResult;
use vmcore::synic::GuestEventPort;

/// Extension trait for [`VcpuFd`] to accept hvdef register types directly.
trait VcpuFdExt {
    fn get_hvdef_regs(&self, regs: &mut [HvRegisterAssoc]) -> Result<(), KernelError>;
    fn set_hvdef_regs(&self, regs: &[HvRegisterAssoc]) -> Result<(), KernelError>;
}

impl VcpuFdExt for VcpuFd {
    fn get_hvdef_regs(&self, regs: &mut [HvRegisterAssoc]) -> Result<(), KernelError> {
        use mshv_bindings::hv_register_assoc;
        const {
            assert!(size_of::<HvRegisterAssoc>() == size_of::<hv_register_assoc>());
            assert!(align_of::<HvRegisterAssoc>() >= align_of::<hv_register_assoc>());
        }
        // SAFETY: HvRegisterAssoc and hv_register_assoc have the same layout.
        self.get_reg(unsafe {
            std::mem::transmute::<&mut [HvRegisterAssoc], &mut [hv_register_assoc]>(regs)
        })?;
        Ok(())
    }

    fn set_hvdef_regs(&self, regs: &[HvRegisterAssoc]) -> Result<(), KernelError> {
        use mshv_bindings::hv_register_assoc;
        const {
            assert!(size_of::<HvRegisterAssoc>() == size_of::<hv_register_assoc>());
            assert!(align_of::<HvRegisterAssoc>() >= align_of::<hv_register_assoc>());
        }
        // SAFETY: HvRegisterAssoc and hv_register_assoc have the same layout.
        self.set_reg(unsafe {
            std::mem::transmute::<&[HvRegisterAssoc], &[hv_register_assoc]>(regs)
        })?;
        Ok(())
    }
}

/// Hypervisor backend for Linux /dev/mshv.
#[derive(Debug)]
pub struct LinuxMshv {
    mshv: Mshv,
}

impl LinuxMshv {
    /// Creates a new instance of the LinuxMshv hypervisor backend.
    pub fn new() -> io::Result<Self> {
        let file = fs_err::File::open("/dev/mshv")?;
        Ok(Self::from(std::fs::File::from(file)))
    }
}

impl From<std::fs::File> for LinuxMshv {
    fn from(file: std::fs::File) -> Self {
        LinuxMshv {
            // SAFETY: We take ownership of the file descriptor and pass it to Mshv.
            // TODO: fix mshv_bindings to not need this unsafe code.
            mshv: unsafe { Mshv::new_with_fd_number(file.into_raw_fd()) },
        }
    }
}

impl<'a> MshvProtoPartition<'a> {
    /// Performs the post-init partition setup common to both architectures:
    /// creates VPs, BSP, installs intercepts, sets up the signal handler,
    /// and checks for unsupported VTL2 configuration.
    fn new(config: ProtoPartitionConfig<'a>, vmfd: VmFd) -> Result<Self, Error> {
        if config.processor_topology.vp_count() > u8::MAX as u32 {
            return Err(ErrorInner::TooManyVps(config.processor_topology.vp_count()).into());
        }

        let vps = config
            .processor_topology
            .vps_arch()
            .map(|vp| MshvVpInner {
                vp_info: vp,
                thread: RwLock::new(None),
                needs_yield: NeedsYield::new(),
                message_queues: MessageQueues::new(),
                message_queues_pending: AtomicBool::new(false),
                waker: RwLock::new(None),
            })
            .collect();

        let bsp = vmfd
            .create_vcpu(0)
            .map_err(|e| ErrorInner::CreateVcpu(e.into()))?;

        // Install intercepts required by both architectures.
        vmfd.install_intercept(mshv_install_intercept {
            access_type_mask: HV_INTERCEPT_ACCESS_MASK_EXECUTE,
            intercept_type: hvdef::hypercall::HvInterceptType::HvInterceptTypeHypercall.0,
            intercept_parameter: Default::default(),
        })
        .map_err(|e| ErrorInner::InstallIntercept(e.into()))?;

        vmfd.install_intercept(mshv_install_intercept {
            access_type_mask: HV_INTERCEPT_ACCESS_MASK_EXECUTE,
            intercept_type:
                hvdef::hypercall::HvInterceptType::HvInterceptTypeUnknownSynicConnection.0,
            intercept_parameter: Default::default(),
        })
        .map_err(|e| ErrorInner::InstallIntercept(e.into()))?;

        vmfd.install_intercept(mshv_install_intercept {
            access_type_mask: HV_INTERCEPT_ACCESS_MASK_EXECUTE,
            intercept_type:
                hvdef::hypercall::HvInterceptType::HvInterceptTypeRetargetInterruptWithUnknownDeviceId.0,
            intercept_parameter: Default::default(),
        })
        .map_err(|e| ErrorInner::InstallIntercept(e.into()))?;

        // Set up a signal for forcing vcpufd.run() to exit with EINTR.
        static SIGNAL_HANDLER_INIT: Once = Once::new();
        // SAFETY: The signal handler does not perform any actions that are
        // forbidden for signal handlers to perform, as it performs nothing.
        SIGNAL_HANDLER_INIT.call_once(|| unsafe {
            signal_hook::low_level::register(libc::SIGRTMIN(), || {
                // Signal handler does nothing other than enabling run_fd()
                // ioctl to return with EINTR, when the associated signal is
                // sent to run_fd() thread.
            })
            .unwrap();
        });

        if let Some(hv_config) = &config.hv_config {
            if hv_config.vtl2.is_some() {
                return Err(ErrorInner::Vtl2NotSupported.into());
            }
        }

        Ok(MshvProtoPartition {
            config,
            vmfd,
            vps,
            bsp,
        })
    }
}

/// Returns whether MSHV is available on this machine.
pub fn is_available() -> Result<bool, Error> {
    match std::fs::metadata("/dev/mshv") {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(ErrorInner::AvailableCheck(err).into()),
    }
}

/// Prototype partition.
pub struct MshvProtoPartition<'a> {
    config: ProtoPartitionConfig<'a>,
    vmfd: VmFd,
    vps: Vec<MshvVpInner>,
    bsp: VcpuFd,
}

/// A partition running on the /dev/mshv hypervisor.
#[derive(Inspect)]
pub struct MshvPartition {
    #[inspect(flatten)]
    inner: Arc<MshvPartitionInner>,
    #[inspect(skip)]
    synic_ports: Arc<virt::synic::SynicPorts<MshvPartitionInner>>,
}

#[derive(Inspect)]
struct MshvPartitionInner {
    #[inspect(skip)]
    vmfd: VmFd,
    /// The BSP's VcpuFd, retained for partition-level register access
    /// (VM state get/set). Only used while VPs are stopped.
    #[inspect(skip)]
    bsp_vcpufd: VcpuFd,
    #[inspect(skip)]
    memory: Mutex<MshvMemoryRangeState>,
    gm: GuestMemory,
    mem_layout: vm_topology::memory::MemoryLayout,
    #[inspect(skip)]
    vps: Vec<MshvVpInner>,
    #[cfg(guest_arch = "x86_64")]
    irq_routes: virt::irqcon::IrqRoutes,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(skip)]
    gsi_states: Mutex<Box<[irqfd::GsiState; irqfd::NUM_GSIS]>>,
    caps: virt::PartitionCapabilities,
    synic_ports: virt::synic::SynicPortMap,
    #[cfg(guest_arch = "x86_64")]
    cpuid: virt::CpuidLeafSet,
    #[cfg(guest_arch = "x86_64")]
    software_devices: virt::x86::apic_software_device::ApicSoftwareDevices,
    /// Set to `true` when partition time is frozen (e.g. during reset).
    /// The first VP to enter `run_vp` after a freeze will thaw time.
    time_frozen: Mutex<bool>,
}

struct MshvVpInner {
    vp_info: vm_topology::processor::TargetVpInfo,
    thread: RwLock<Option<Pthread>>,
    needs_yield: NeedsYield,
    message_queues: MessageQueues,
    /// Set by device threads after enqueuing a message to signal the VP
    /// thread to flush its message queues.
    message_queues_pending: AtomicBool,
    /// Waker for the VP run loop task. Set by the VP thread, used by device
    /// threads to re-poll the run loop when new messages are enqueued.
    waker: RwLock<Option<Waker>>,
}

struct MshvVpInnerCleaner<'a> {
    vpinner: &'a MshvVpInner,
}

impl Drop for MshvVpInnerCleaner<'_> {
    fn drop(&mut self) {
        self.vpinner.thread.write().take();
    }
}

impl GetReferenceTime for MshvPartitionInner {
    fn now(&self) -> ReferenceTimeResult {
        // Use the partition property instead of a VP register to avoid
        // deadlocking when VPs are running.
        let ref_time = self
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::ReferenceTime.0)
            .unwrap();
        ReferenceTimeResult {
            ref_time,
            system_time: None,
        }
    }
}

impl MshvPartitionInner {
    fn vp(&self, vp_index: VpIndex) -> &MshvVpInner {
        &self.vps[vp_index.index() as usize]
    }

    /// Freezes partition time. Time will remain frozen until [`thaw_time`] is
    /// called (typically on the first VP run after reset).
    fn freeze_time(&self) -> Result<(), Error> {
        let mut frozen = self.time_frozen.lock();
        if !*frozen {
            self.vmfd
                .set_partition_property(HvPartitionPropertyCode::TimeFreeze.0, 1)
                .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;
            *frozen = true;
        }
        Ok(())
    }

    /// Thaws partition time if it is currently frozen. This is a no-op if
    /// time is already running.
    fn thaw_time(&self) -> Result<(), Error> {
        let mut frozen = self.time_frozen.lock();
        if *frozen {
            self.vmfd
                .set_partition_property(HvPartitionPropertyCode::TimeFreeze.0, 0)
                .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;
            *frozen = false;
        }
        Ok(())
    }

    fn post_message(&self, vp_index: VpIndex, sint: u8, message: &HvMessage) {
        let vp = self.vp(vp_index);
        let wake = vp.message_queues.enqueue_message(sint, message);
        // Signal the VP thread to flush message queues.
        if wake && !vp.message_queues_pending.swap(true, Ordering::Release) {
            if let Some(waker) = &*vp.waker.read() {
                waker.wake_by_ref();
            }
        }
    }

    /// Posts a message directly to a VP's SynIC sint.
    ///
    /// This wraps the HvCallPostMessageDirect hypercall via the raw hvcall
    /// interface. This is used instead of the `mshv-ioctls` method because
    /// that method is only available on x86.
    // TODO: upstream an arch-independent version to mshv-ioctls.
    fn post_message_direct(&self, vp: u32, sint: u8, message: &HvMessage) -> Result<(), MshvError> {
        use mshv_bindings::mshv_root_hvcall;

        let post_message = hvdef::hypercall::PostMessageDirect {
            partition_id: 0,
            vp_index: vp,
            vtl: Vtl::Vtl0 as u8,
            padding0: [0; 3],
            sint,
            padding1: [0; 3],
            message: zerocopy::Unalign::new(*message),
            padding2: 0,
        };

        let mut args = mshv_root_hvcall {
            code: hvdef::HypercallCode::HvCallPostMessageDirect.0,
            in_sz: size_of::<hvdef::hypercall::PostMessageDirect>() as u16,
            in_ptr: std::ptr::addr_of!(post_message) as u64,
            ..Default::default()
        };
        self.vmfd.hvcall(&mut args)
    }

    /// Signals a SynIC event directly on a VP.
    ///
    /// This wraps the HvCallSignalEventDirect hypercall via the raw hvcall
    /// interface. This is used instead of the `mshv-ioctls` method because
    /// that method is only available on x86.
    // TODO: upstream an arch-independent version to mshv-ioctls.
    fn signal_event_direct(&self, vp: u32, sint: u8, flag: u16) -> Result<(), MshvError> {
        use mshv_bindings::mshv_root_hvcall;
        use zerocopy::FromZeros;

        let input = hvdef::hypercall::SignalEventDirect {
            target_partition: 0,
            target_vp: vp,
            target_vtl: 0,
            target_sint: sint,
            flag_number: flag,
        };
        let mut output = hvdef::hypercall::SignalEventDirectOutput::new_zeroed();

        let mut args = mshv_root_hvcall {
            code: hvdef::HypercallCode::HvCallSignalEventDirect.0,
            in_sz: size_of::<hvdef::hypercall::SignalEventDirect>() as u16,
            out_sz: size_of::<hvdef::hypercall::SignalEventDirectOutput>() as u16,
            in_ptr: std::ptr::addr_of!(input) as u64,
            out_ptr: std::ptr::addr_of_mut!(output) as u64,
            ..Default::default()
        };
        self.vmfd.hvcall(&mut args)
    }
}

/// Binds a virtual processor to the current thread.
pub struct MshvProcessorBinder {
    partition: Arc<MshvPartitionInner>,
    vcpufd: Option<VcpuFd>,
    vpindex: VpIndex,
}

/// Wraps a VcpuFd for running a VP. On x86_64, also provides access to the
/// register page for fast register reads/writes.
struct MshvVpRunner<'a> {
    vcpufd: &'a VcpuFd,
    #[cfg(guest_arch = "x86_64")]
    reg_page: *mut hvdef::HvX64RegisterPage,
}

impl MshvVpRunner<'_> {
    fn run(&mut self) -> Result<HvMessage, MshvError> {
        self.vcpufd.run().map(|msg| {
            // SAFETY: hv_message and HvMessage have the same size
            // (256 bytes) and compatible layout (header + 240-byte
            // payload).
            unsafe { std::mem::transmute::<mshv_bindings::hv_message, HvMessage>(msg) }
        })
    }

    #[cfg(guest_arch = "x86_64")]
    fn reg_page(&mut self) -> &mut hvdef::HvX64RegisterPage {
        // SAFETY: VP is stopped (returned from run()), so we have exclusive
        // access. The raw pointer was obtained from the kernel's mmap of
        // the register page and remains valid for the VP's lifetime.
        unsafe { &mut *self.reg_page }
    }
}

/// A bound virtual processor for the /dev/mshv hypervisor.
#[derive(InspectMut)]
pub struct MshvProcessor<'a> {
    #[inspect(skip)]
    partition: &'a MshvPartitionInner,
    #[inspect(skip)]
    inner: &'a MshvVpInner,
    #[inspect(skip)]
    vpindex: VpIndex,
    #[inspect(skip)]
    runner: MshvVpRunner<'a>,
    /// The deliverability notification state currently registered with the
    /// hypervisor.
    #[inspect(skip)]
    deliverability_notifications: HvDeliverabilityNotificationsRegister,
}

impl MshvProcessor<'_> {
    /// Posts any queued messages for the given sints, and requests
    /// deliverability notifications for any sints that still have pending
    /// messages.
    fn flush_messages(&mut self, deliverable_sints: u16) {
        let nonempty_sints =
            self.inner
                .message_queues
                .post_pending_messages(deliverable_sints, |sint, message| {
                    match self
                        .partition
                        .post_message_direct(self.vpindex.index(), sint, message)
                    {
                        Ok(()) => {
                            tracing::trace!(sint, "sint message posted successfully");
                            Ok(())
                        }
                        Err(e) => {
                            tracelimit::warn_ratelimited!(
                                error = &e as &dyn std::error::Error,
                                "dropping sint message"
                            );
                            Err(HvError::ObjectInUse)
                        }
                    }
                });

        if self.deliverability_notifications.sints() != nonempty_sints {
            let notifications = self.deliverability_notifications.with_sints(nonempty_sints);
            tracing::trace!(?notifications, "setting deliverability notifications");
            self.partition
                .vmfd
                .register_deliverabilty_notifications(
                    self.vpindex.index(),
                    u64::from(notifications),
                )
                .expect("requesting deliverability is not a fallible operation");
            self.deliverability_notifications = notifications;
        }
    }

    /// Handles a synic sint deliverable exit. The deliverable sints bitmap
    /// is architecture-specific (different message types for x86_64 and
    /// aarch64), so the caller extracts it and passes it here.
    fn handle_sint_deliverable(&mut self, deliverable_sints: u16) {
        // Clear the delivered sints from both the current and next state.
        self.deliverability_notifications
            .set_sints(self.deliverability_notifications.sints() & !deliverable_sints);

        self.flush_messages(deliverable_sints);
    }

    /// Resets the VP's message queue and deliverability notification state.
    fn reset_synic_state(&mut self) {
        self.inner.message_queues.clear();
        self.inner
            .message_queues_pending
            .store(false, Ordering::Relaxed);
        self.deliverability_notifications = HvDeliverabilityNotificationsRegister::new();
    }
}

impl virt::Processor for MshvProcessor<'_> {
    type StateAccess<'a>
        = &'a mut Self
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        _state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <&mut Self as virt::vp::AccessVpState>::Error> {
        Err(ErrorInner::NotSupported.into())
    }

    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        let vpinner = self.inner;
        let _cleaner = MshvVpInnerCleaner { vpinner };

        assert!(vpinner.thread.write().replace(Pthread::current()).is_none());

        self.partition
            .thaw_time()
            .expect("failed to thaw partition time");

        // Ensure any messages present from a state restore are flushed on
        // the first loop iteration.
        if vpinner.message_queues.pending_sints() != 0 {
            vpinner
                .message_queues_pending
                .store(true, Ordering::Relaxed);
        }

        let mut last_waker: Option<Waker> = None;

        loop {
            vpinner.needs_yield.maybe_yield().await;
            stop.check()?;

            // Ensure the waker is set so device threads can wake us.
            poll_fn(|cx| {
                if !last_waker.as_ref().is_some_and(|w| cx.waker().will_wake(w)) {
                    last_waker = Some(cx.waker().clone());
                    *vpinner.waker.write() = last_waker.clone();
                }
                std::task::Poll::Ready(())
            })
            .await;

            // Flush any messages enqueued by device threads.
            if vpinner.message_queues_pending.load(Ordering::Relaxed) {
                vpinner
                    .message_queues_pending
                    .store(false, Ordering::SeqCst);
                let pending_sints = vpinner.message_queues.pending_sints();
                if pending_sints != 0 {
                    self.flush_messages(pending_sints);
                }
            }

            match self.runner.run() {
                Ok(exit) => {
                    self.handle_exit(&exit, dev).await?;
                }
                Err(e) => match e.errno() {
                    libc::EAGAIN | libc::EINTR => {}
                    _ => tracing::error!(
                        error = &e as &dyn std::error::Error,
                        "vcpufd.run returned error"
                    ),
                },
            }
        }
    }

    fn flush_async_requests(&mut self) {}

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);
        self
    }

    fn reset(&mut self) -> Result<(), impl std::error::Error + Send + Sync + 'static> {
        use virt::vp::AccessVpState;

        let vp_info = self.inner.vp_info;
        self.access_state(Vtl::Vtl0)
            .reset_all(&vp_info)
            .map_err(|e| ErrorInner::ResetState(Box::new(e)))?;

        self.reset_synic_state();

        Ok::<(), Error>(())
    }
}

impl hv1_hypercall::PostMessage for arch::MshvHypercallHandler<'_> {
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> hvdef::HvResult<()> {
        self.partition
            .synic_ports
            .handle_post_message(Vtl::Vtl0, connection_id, false, message)
    }
}

impl hv1_hypercall::SignalEvent for arch::MshvHypercallHandler<'_> {
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        self.partition
            .synic_ports
            .handle_signal_event(Vtl::Vtl0, connection_id, flag)
    }
}

/// Error type for /dev/mshv operations.
#[derive(Error, Debug)]
#[error(transparent)]
pub struct Error(ErrorInner);

impl<T: Into<ErrorInner>> From<T> for Error {
    fn from(err: T) -> Self {
        Error(err.into())
    }
}

// TODO: Chunk this up into smaller types.
#[derive(Error, Debug)]
enum ErrorInner {
    #[error("operation not supported")]
    NotSupported,
    #[error("create_vm failed")]
    CreateVMFailed,
    #[error("failed to initialize VM")]
    CreateVMInitFailed(#[source] anyhow::Error),
    #[error("failed to create VCPU")]
    CreateVcpu(#[source] KernelError),
    #[error("vtl2 not supported")]
    Vtl2NotSupported,
    #[error("isolation not supported")]
    IsolationNotSupported,
    #[error("failed to stat /dev/mshv")]
    AvailableCheck(#[source] io::Error),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to get partition property")]
    GetPartitionProperty(#[source] KernelError),
    #[error("failed to set partition property")]
    SetPartitionProperty(#[source] KernelError),
    #[error("register access error")]
    Register(#[source] KernelError),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to get VP state {ty}")]
    GetVpState {
        #[source]
        error: KernelError,
        ty: u8,
    },
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to set VP state {ty}")]
    SetVpState {
        #[source]
        error: KernelError,
        ty: u8,
    },
    #[error("failed to reset state")]
    ResetState(#[source] Box<virt::state::StateError<Error>>),
    #[error("install intercept failed")]
    InstallIntercept(#[source] KernelError),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to register cpuid override")]
    RegisterCpuid(#[source] KernelError),
    #[cfg(guest_arch = "x86_64")]
    #[error("host does not support required cpu capabilities")]
    Capabilities(#[source] virt::PartitionCapabilitiesError),
    #[error("too many virtual processors: {0}")]
    TooManyVps(u32),
    #[cfg(guest_arch = "x86_64")]
    #[error("unsupported processor vendor: {0:?}")]
    UnsupportedProcessorVendor(hvdef::HvProcessorVendor),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to create virtual device")]
    NewDevice(#[source] virt::x86::apic_software_device::DeviceIdInUse),
}

/// Equivalent to [`MshvError`] but has a much better error message.
#[derive(Error, Debug)]
enum KernelError {
    #[error("kernel error")]
    Kernel(#[source] io::Error),
    #[error("hypercall {code:#x?} error")]
    Hypercall {
        code: hvdef::HypercallCode,
        #[source]
        error: HvError,
    },
}

impl From<MshvError> for KernelError {
    fn from(err: MshvError) -> Self {
        match err {
            MshvError::Errno(e) => KernelError::Kernel(e.into()),
            MshvError::Hypercall {
                code,
                status_raw,
                status: _,
            } => KernelError::Hypercall {
                code: hvdef::HypercallCode(code),
                error: HvError::from(
                    std::num::NonZeroU16::new(status_raw)
                        .expect("not an error, hypercall returned success"),
                ),
            },
        }
    }
}

/// Creates a VM with retry on EINTR.
fn create_vm_with_retry(
    mshv: &Mshv,
    args: &mshv_bindings::mshv_create_partition_v2,
) -> Result<VmFd, Error> {
    loop {
        match mshv.create_vm_with_args(args) {
            Ok(fd) => return Ok(fd),
            Err(e) => {
                if e.errno() == libc::EINTR {
                    continue;
                } else {
                    return Err(ErrorInner::CreateVMFailed.into());
                }
            }
        }
    }
}

/// Returns the base set of synthetic processor features shared by both
/// architectures. Each architecture may add extra features before passing
/// the result to `set_partition_property`.
fn common_synthetic_features() -> hvdef::HvPartitionSyntheticProcessorFeatures {
    hvdef::HvPartitionSyntheticProcessorFeatures::new()
        .with_hypervisor_present(true)
        .with_hv1(true)
        .with_access_vp_run_time_reg(true)
        .with_access_partition_reference_counter(true)
        .with_access_synic_regs(true)
        .with_access_synthetic_timer_regs(true)
        .with_access_intr_ctrl_regs(true)
        .with_access_hypercall_regs(true)
        .with_access_vp_index(true)
        .with_fast_hypercall_output(true)
        .with_direct_synthetic_timers(true)
        .with_extended_processor_masks(true)
        .with_tb_flush_hypercalls(true)
        .with_synthetic_cluster_ipi(true)
        .with_notify_long_spin_wait(true)
        .with_query_numa_distance(true)
        .with_signal_events(true)
        .with_retarget_device_interrupt(true)
}

impl PartitionAccessState for MshvPartition {
    type StateAccess<'a> = &'a MshvPartition;

    fn access_state(&self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);
        self
    }
}

#[derive(Debug, Default)]
struct MshvMemoryRangeState {
    ranges: Vec<Option<mshv_user_mem_region>>,
}

impl virt::PartitionMemoryMapper for MshvPartition {
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        assert_eq!(vtl, Vtl::Vtl0);
        self.inner.clone()
    }
}

// TODO: figure out a better abstraction that also works for KVM and WHP.
impl virt::PartitionMemoryMap for MshvPartitionInner {
    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()> {
        let mut state = self.memory.lock();

        // Memory slots cannot be resized but can be moved within the guest
        // address space. Find the existing slot if there is one.
        let mut slot_to_use = None;
        for (slot, range) in state.ranges.iter_mut().enumerate() {
            match range {
                Some(range) if range.userspace_addr == data as u64 => {
                    slot_to_use = Some(slot);
                    break;
                }
                Some(_) => (),
                None => slot_to_use = Some(slot),
            }
        }
        if slot_to_use.is_none() {
            slot_to_use = Some(state.ranges.len());
            state.ranges.push(None);
        }
        let slot_to_use = slot_to_use.unwrap();

        let mut flags = 0;
        if writable {
            flags |= set_bits!(u8, MSHV_SET_MEM_BIT_WRITABLE);
        }
        if exec {
            flags |= set_bits!(u8, MSHV_SET_MEM_BIT_EXECUTABLE);
        }
        let mem_region = mshv_user_mem_region {
            size: size as u64,
            guest_pfn: addr >> HV_PAGE_SHIFT,
            userspace_addr: data as u64,
            flags,
            rsvd: [0; 7],
        };

        let _span = tracing::info_span!(
            "mshv map user memory",
            guest_pfn = mem_region.guest_pfn,
            size = mem_region.size,
            writable,
            exec,
        )
        .entered();
        self.vmfd.map_user_memory(mem_region)?;
        state.ranges[slot_to_use] = Some(mem_region);
        Ok(())
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let unmap_start = addr >> HV_PAGE_SHIFT;
        let unmap_end = (addr + size) >> HV_PAGE_SHIFT;
        let mut state = self.memory.lock();
        for entry in &mut state.ranges {
            let Some(region) = entry.as_ref() else {
                continue;
            };
            let region_start = region.guest_pfn;
            let region_end = region.guest_pfn + (region.size >> HV_PAGE_SHIFT);
            if unmap_start <= region_start && region_end <= unmap_end {
                // Region is fully contained in the unmap range.
                let _span = tracing::info_span!(
                    "mshv unmap user memory",
                    guest_pfn = region.guest_pfn,
                    size = region.size,
                )
                .entered();
                self.vmfd.unmap_user_memory(*region)?;
                *entry = None;
            } else {
                assert!(
                    region_end <= unmap_start || unmap_end <= region_start,
                    "unmap range partially overlaps a mapped region"
                );
            }
        }
        Ok(())
    }
}

/// Holds the state needed to deassign an MSHV ioeventfd on drop.
///
/// The kernel's `mshv_deassign_ioeventfd` matches entries by (eventfd,
/// addr, len, datamatch/wildcard), so we must keep all of these alive
/// for the deassign ioctl.
struct MshvDoorbellEntry {
    partition: Weak<MshvPartitionInner>,
    event: Event,
    guest_address: u64,
    datamatch: u64,
    len: u32,
    flags: u32,
}

impl MshvDoorbellEntry {
    fn new(
        partition: &Arc<MshvPartitionInner>,
        guest_address: u64,
        value: Option<u64>,
        length: Option<u32>,
        fd: &Event,
    ) -> io::Result<MshvDoorbellEntry> {
        let flags = if value.is_some() {
            1 << mshv_bindings::MSHV_IOEVENTFD_BIT_DATAMATCH
        } else {
            0
        };
        let datamatch = value.unwrap_or(0);
        let len = length.unwrap_or(0);
        let event = fd.clone();

        let ioeventfd = mshv_bindings::mshv_user_ioeventfd {
            datamatch,
            addr: guest_address,
            len,
            fd: event.as_fd().as_raw_fd(),
            flags,
            ..Default::default()
        };
        // SAFETY: `partition.vmfd` is valid because it is owned by
        // `MshvPartitionInner`. The `ioeventfd` struct is properly
        // initialized on the stack.
        let ret = unsafe {
            libc::ioctl(
                partition.vmfd.as_raw_fd(),
                mshv_ioctls::MSHV_IOEVENTFD() as _,
                std::ptr::from_ref(&ioeventfd),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            partition: Arc::downgrade(partition),
            event,
            guest_address,
            datamatch,
            len,
            flags,
        })
    }
}

impl Drop for MshvDoorbellEntry {
    fn drop(&mut self) {
        if let Some(partition) = self.partition.upgrade() {
            let ioeventfd = mshv_bindings::mshv_user_ioeventfd {
                datamatch: self.datamatch,
                addr: self.guest_address,
                len: self.len,
                fd: self.event.as_fd().as_raw_fd(),
                flags: self.flags | (1 << mshv_bindings::MSHV_IOEVENTFD_BIT_DEASSIGN),
                ..Default::default()
            };
            // SAFETY: `partition.vmfd` is valid because we successfully
            // upgraded the weak reference. The `ioeventfd` struct is
            // properly initialized on the stack.
            let ret = unsafe {
                libc::ioctl(
                    partition.vmfd.as_raw_fd(),
                    mshv_ioctls::MSHV_IOEVENTFD() as _,
                    std::ptr::from_ref(&ioeventfd),
                )
            };
            assert!(
                ret >= 0,
                "failed to unregister doorbell at {:#x}: {}",
                self.guest_address,
                io::Error::last_os_error()
            );
        }
    }
}

impl DoorbellRegistration for MshvPartition {
    fn register_doorbell(
        &self,
        guest_address: u64,
        value: Option<u64>,
        length: Option<u32>,
        fd: &Event,
    ) -> io::Result<Box<dyn Send + Sync>> {
        Ok(Box::new(MshvDoorbellEntry::new(
            &self.inner,
            guest_address,
            value,
            length,
            fd,
        )?))
    }
}

impl virt::synic::Synic for MshvPartitionInner {
    fn port_map(&self) -> &virt::synic::SynicPortMap {
        &self.synic_ports
    }

    fn post_message(&self, _vtl: Vtl, vp: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        self.post_message(vp, sint, &HvMessage::new(HvMessageType(typ), 0, payload));
    }

    fn new_guest_event_port(
        self: Arc<Self>,
        _vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort> {
        Box::new(MshvGuestEventPort {
            partition: Arc::downgrade(&self),
            params: Arc::new(Mutex::new(MshvEventPortParams {
                vp: VpIndex::new(vp),
                sint,
                flag,
            })),
        })
    }

    fn prefer_os_events(&self) -> bool {
        false
    }
}

/// `GuestEventPort` implementation for MSHV partitions.
#[derive(Debug, Clone)]
struct MshvGuestEventPort {
    partition: Weak<MshvPartitionInner>,
    params: Arc<Mutex<MshvEventPortParams>>,
}

#[derive(Debug, Copy, Clone)]
struct MshvEventPortParams {
    vp: VpIndex,
    sint: u8,
    flag: u16,
}

impl GuestEventPort for MshvGuestEventPort {
    fn interrupt(&self) -> Interrupt {
        let partition = self.partition.clone();
        let params = self.params.clone();
        Interrupt::from_fn(move || {
            let MshvEventPortParams { vp, sint, flag } = *params.lock();
            if let Some(partition) = partition.upgrade() {
                partition
                    .signal_event_direct(vp.index(), sint, flag)
                    .unwrap_or_else(|_| {
                        panic!(
                            "Failed signal synic sint {} on vp {:?} with flag {}",
                            sint, vp, flag
                        )
                    });
            }
        })
    }

    fn set_target_vp(&mut self, vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
        self.params.lock().vp = VpIndex::new(vp);
        Ok(())
    }
}
