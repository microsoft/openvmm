// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux /dev/mshv implementation of the virt::generic interfaces.

#![cfg(all(target_os = "linux", guest_is_native, guest_arch = "x86_64"))]
#![expect(missing_docs)]
// UNSAFETY: Calling HV APIs and manually managing memory.
#![expect(unsafe_code)]

mod vm_state;
mod vp_state;

use arrayvec::ArrayVec;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use hv1_emulator::message_queues::MessageQueues;
use hv1_hypercall::X64RegisterIo;
use hvdef::HV_PAGE_SHIFT;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::HvX64RegisterName;
use hvdef::HvX64VpExecutionState;
use hvdef::Vtl;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_EXECUTE;
use hvdef::hypercall::HvRegisterAssoc;
use inspect::Inspect;
use inspect::InspectMut;
use mshv_bindings::MSHV_SET_MEM_BIT_EXECUTABLE;
use mshv_bindings::MSHV_SET_MEM_BIT_WRITABLE;
use mshv_bindings::hv_message;
use mshv_bindings::hv_register_assoc;
use mshv_bindings::hv_register_value;
use mshv_bindings::hv_u128;
use mshv_bindings::hv_x64_io_port_intercept_message;
use mshv_bindings::hv_x64_memory_intercept_message;
use mshv_bindings::hv_x64_segment_register;
use mshv_bindings::mshv_install_intercept;
use mshv_bindings::mshv_user_mem_region;
use mshv_ioctls::InterruptRequest;
use mshv_ioctls::Mshv;
use mshv_ioctls::MshvError;
use mshv_ioctls::VcpuFd;
use mshv_ioctls::VmFd;
use mshv_ioctls::set_bits;
use mshv_ioctls::set_registers_64;
use pal::unix::pthread::*;
use pal_event::Event;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::convert::Infallible;
use std::io;
use std::sync::Arc;
use std::sync::Once;
use std::sync::Weak;
use thiserror::Error;
use virt::Hv1;
use virt::NeedsYield;
use virt::PartitionAccessState;
use virt::PartitionConfig;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::irqcon::MsiRequest;
use virt::x86::max_physical_address_size_from_cpuid;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::EmulatorSupport;
use virt_support_x86emu::emulate::TranslateGvaSupport;
use virt_support_x86emu::emulate::TranslateMode;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::translate::TranslationRegisters;
use vmcore::interrupt::Interrupt;
use vmcore::reference_time::GetReferenceTime;
use vmcore::reference_time::ReferenceTimeResult;
use vmcore::reference_time::ReferenceTimeSource;
use vmcore::synic::GuestEventPort;
use x86defs::RFlags;
use x86defs::SegmentRegister;
use zerocopy::IntoBytes;

#[derive(Debug)]
pub struct LinuxMshv;

struct MshvEmuCache {
    /// GP registers, in the canonical order (as defined by `RAX`, etc.).
    gps: [u64; 16],
    /// Segment registers, in the canonical order (as defined by `ES`, etc.).
    segs: [SegmentRegister; 6],
    rip: u64,
    rflags: RFlags,

    cr0: u64,
    efer: u64,
}

impl virt::Hypervisor for LinuxMshv {
    type ProtoPartition<'a> = MshvProtoPartition<'a>;
    type Partition = MshvPartition;
    type Error = Error;

    fn new_partition<'a>(
        &mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<MshvProtoPartition<'a>, Self::Error> {
        if config.isolation.is_isolated() {
            return Err(Error::IsolationNotSupported);
        }

        // Open /dev/mshv.
        let mshv = Mshv::new().map_err(Error::OpenMshv)?;

        // Create VM.
        //
        // TODO: really need to pass some partition properties here (e.g., for
        // APIC configuration), but the underlying crate just hardcodes
        // everything.
        let vmfd: VmFd;
        loop {
            match mshv.create_vm() {
                Ok(fd) => vmfd = fd,
                Err(e) => {
                    if e.errno() == libc::EINTR {
                        // If the error returned is EINTR, which means the
                        // ioctl has been interrupted, we have to retry as
                        // this can't be considered as a regular error.
                        continue;
                    } else {
                        return Err(Error::CreateVMFailed);
                    }
                }
            }
            break;
        }

        vmfd.initialize()
            .map_err(|e| Error::CreateVMInitFailed(e.into()))?;

        // Create virtual CPUs.
        let mut vps: Vec<MshvVpInner> = Vec::new();
        for vp in config.processor_topology.vps_arch() {
            if vp.base.vp_index.index() != vp.apic_id {
                // TODO
                return Err(Error::NotSupported);
            }

            let vcpufd = vmfd
                .create_vcpu(vp.base.vp_index.index() as u8)
                .map_err(Error::CreateVcpu)?;

            vps.push(MshvVpInner {
                vcpufd,
                thread: RwLock::new(None),
                needs_yield: NeedsYield::new(),
                message_queues: MessageQueues::new(),
                deliverability_notifications: Mutex::new(
                    HvDeliverabilityNotificationsRegister::new(),
                ),
            });
        }

        // Install required intercepts
        let intercept_args = mshv_install_intercept {
            access_type_mask: HV_INTERCEPT_ACCESS_MASK_EXECUTE,
            intercept_type: hvdef::hypercall::HvInterceptType::HvInterceptTypeHypercall.0,
            intercept_parameter: Default::default(),
        };
        vmfd.install_intercept(intercept_args)
            .map_err(Error::InstallIntercept)?;

        // Set up a signal for forcing vcpufd.run() ioctl to exit.
        static SIGNAL_HANDLER_INIT: Once = Once::new();
        // SAFETY: The signal handler does not perform any actions that are forbidden
        // for signal handlers to perform, as it performs nothing.
        SIGNAL_HANDLER_INIT.call_once(|| unsafe {
            signal_hook::low_level::register(libc::SIGRTMIN(), || {
                // Signal handler does nothing other than enabling run_fd() iotcl to
                // return with EINTR, when the associated signal is sent to run_fd() thread.
            })
            .unwrap();
        });

        if let Some(hv_config) = &config.hv_config {
            if hv_config.vtl2.is_some() {
                return Err(Error::Vtl2NotSupported);
            }
        }

        Ok(MshvProtoPartition { config, vmfd, vps })
    }

    fn is_available(&self) -> Result<bool, Self::Error> {
        match std::fs::metadata("/dev/mshv") {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::AvailableCheck(err)),
        }
    }
}

/// Prototype partition.
pub struct MshvProtoPartition<'a> {
    config: ProtoPartitionConfig<'a>,
    vmfd: VmFd,
    vps: Vec<MshvVpInner>,
}

impl ProtoPartition for MshvProtoPartition<'_> {
    type Partition = MshvPartition;
    type ProcessorBinder = MshvProcessorBinder;
    type Error = Error;

    fn cpuid(&self, eax: u32, ecx: u32) -> [u32; 4] {
        // This call should never fail unless there is a kernel or hypervisor
        // bug.
        self.vps[0]
            .vcpufd
            .get_cpuid_values(eax, ecx, 0, 0)
            .expect("cpuid should not fail")
    }

    fn max_physical_address_size(&self) -> u8 {
        max_physical_address_size_from_cpuid(&|eax, ecx| self.cpuid(eax, ecx))
    }

    fn build(
        self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        // TODO: do something with cpuid.

        // Get caps via cpuid
        let caps = virt::PartitionCapabilities::from_cpuid(
            self.config.processor_topology,
            &mut |function, index| {
                self.vps[0]
                    .vcpufd
                    .get_cpuid_values(function, index, 0, 0)
                    .expect("cpuid should not fail")
            },
        )
        .map_err(Error::Capabilities)?;

        // Attach all the resources created above to a Partition object.
        let partition = MshvPartition {
            inner: Arc::new(MshvPartitionInner {
                vmfd: self.vmfd,
                memory: Default::default(),
                gm: config.guest_memory.clone(),
                vps: self.vps,
                irq_routes: Default::default(),
                caps,
            }),
        };

        let vps = self
            .config
            .processor_topology
            .vps()
            .map(|vp| MshvProcessorBinder {
                partition: partition.inner.clone(),
                vpindex: vp.vp_index,
            })
            .collect();

        Ok((partition, vps))
    }
}

// TODO: remove these workarounds when mshv-ioctl implements the Debug trait
#[derive(Debug)]
pub struct MshvPartition {
    inner: Arc<MshvPartitionInner>,
}

#[derive(Debug)]
struct MshvPartitionInner {
    vmfd: VmFd,
    memory: Mutex<MshvMemoryRangeState>,
    gm: GuestMemory,
    vps: Vec<MshvVpInner>,
    irq_routes: virt::irqcon::IrqRoutes,
    caps: virt::PartitionCapabilities,
}

#[derive(Debug)]
struct MshvVpInner {
    vcpufd: VcpuFd,
    thread: RwLock<Option<Pthread>>,
    needs_yield: NeedsYield,
    message_queues: MessageQueues,
    deliverability_notifications: Mutex<HvDeliverabilityNotificationsRegister>,
}

struct MshvVpInnerCleaner<'a> {
    vpinner: &'a MshvVpInner,
}

impl Drop for MshvVpInnerCleaner<'_> {
    fn drop(&mut self) {
        self.vpinner.thread.write().take();
    }
}

impl virt::Partition for MshvPartition {
    fn supports_reset(&self) -> Option<&dyn virt::ResetPartition<Error = Error>> {
        None
    }

    fn doorbell_registration(
        self: &Arc<Self>,
        _minimum_vtl: Vtl,
    ) -> Option<Arc<dyn DoorbellRegistration>> {
        // TODO: implementation

        Some(self.clone())
    }

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn request_msi(&self, _vtl: Vtl, request: MsiRequest) {
        self.inner.request_msi(request)
    }

    fn request_yield(&self, vp_index: VpIndex) {
        let vp = self.inner.vp(vp_index);
        if vp.needs_yield.request_yield() {
            // Send a signal to the thread who called vcpufd.run() to force an exit.
            let thread = vp.thread.read();
            if let Some(thread) = *thread {
                if thread != Pthread::current() {
                    thread
                        .signal(libc::SIGRTMIN())
                        .expect("thread cancel signal failed");
                }
            }
        }
    }
}

impl virt::X86Partition for MshvPartition {
    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting> {
        self.inner.clone()
    }

    fn pulse_lint(&self, vp_index: VpIndex, vtl: Vtl, lint: u8) {
        // TODO
        tracing::warn!(?vp_index, ?vtl, lint, "ignored lint pulse");
    }
}

impl PartitionAccessState for MshvPartition {
    type StateAccess<'a> = &'a MshvPartition;

    fn access_state(&self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);

        self
    }
}

impl Hv1 for MshvPartition {
    type Error = Error;
    type Device = virt::UnimplementedDevice;

    fn reference_time_source(&self) -> Option<ReferenceTimeSource> {
        Some(ReferenceTimeSource::from(self.inner.clone() as Arc<_>))
    }

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        None
    }
}

impl GetReferenceTime for MshvPartitionInner {
    fn now(&self) -> ReferenceTimeResult {
        let mut regs = [hv_register_assoc {
            name: hvdef::HvAllArchRegisterName::TimeRefCount.0,
            value: hv_register_value { reg64: 0 },
            ..Default::default()
        }];
        self.vp(VpIndex::BSP).vcpufd.get_reg(&mut regs).unwrap();
        // SAFETY: the value has been written by the kernel.
        let ref_time = unsafe { regs[0].value.reg64 };
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

    fn post_message(&self, vp_index: VpIndex, sint: u8, message: &HvMessage) {
        let request_notification = self
            .vp(vp_index)
            .message_queues
            .enqueue_message(sint, message);

        if request_notification {
            self.request_sint_notifications(vp_index, 1 << sint);
        }
    }

    fn request_sint_notifications(&self, vp_index: VpIndex, sints: u16) {
        let mut notifications = self.vp(vp_index).deliverability_notifications.lock();
        if notifications.sints() != sints {
            notifications.set_sints(sints);
            self.vmfd
                .register_deliverabilty_notifications(vp_index.index(), (*notifications).into())
                .expect("Requesting deliverability is not a fallable operation");
        }
    }
}

pub struct MshvProcessorBinder {
    partition: Arc<MshvPartitionInner>,
    vpindex: VpIndex,
}

impl virt::BindProcessor for MshvProcessorBinder {
    type Processor<'a>
        = MshvProcessor<'a>
    where
        Self: 'a;
    type Error = Error;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        Ok(MshvProcessor {
            partition: &self.partition,
            inner: &self.partition.vps[self.vpindex.index() as usize],
            vpindex: self.vpindex,
        })
    }
}

pub struct MshvProcessor<'a> {
    partition: &'a MshvPartitionInner,
    inner: &'a MshvVpInner,
    vpindex: VpIndex,
}

impl MshvProcessor<'_> {
    async fn emulate(
        &self,
        message: &hv_message,
        devices: &impl CpuIo,
        interruption_pending: bool,
    ) -> Result<(), VpHaltReason> {
        let cache = self.emulation_cache();
        let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
            gm: &self.partition.gm,
            kx_gm: &self.partition.gm,
            ux_gm: &self.partition.gm,
        };

        let mut support = MshvEmulationState {
            partition: self.partition,
            processor: self.inner,
            vp_index: self.vpindex,
            message,
            interruption_pending,
            cache,
        };
        virt_support_x86emu::emulate::emulate(&mut support, &emu_mem, devices).await
    }

    async fn handle_io_port_intercept(
        &self,
        message: &hv_message,
        devices: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let info = message.to_ioport_info().unwrap();
        let access_info = info.access_info;
        // SAFETY: This union only contains one field.
        let port_access_info = unsafe { access_info.__bindgen_anon_1 };

        if port_access_info.string_op() != 0 || port_access_info.rep_prefix() != 0 {
            let execution_state = info.header.execution_state;
            // SAFETY: This union only contains one field.
            let io_execution_state = unsafe { execution_state.__bindgen_anon_1 };
            let interruption_pending = io_execution_state.interruption_pending() != 0;

            self.emulate(message, devices, interruption_pending).await?
        } else {
            let mut ret_rax = info.rax;
            virt_support_x86emu::emulate::emulate_io(
                self.vpindex,
                info.header.intercept_access_type == 1,
                info.port_number,
                &mut ret_rax,
                port_access_info.access_size(),
                devices,
            )
            .await;

            let insn_len = info.header.instruction_length() as u64;

            /* Advance RIP and update RAX */
            let arr_reg_name_value = [
                (
                    mshv_bindings::hv_register_name_HV_X64_REGISTER_RIP,
                    info.header.rip + insn_len,
                ),
                (mshv_bindings::hv_register_name_HV_X64_REGISTER_RAX, ret_rax),
            ];

            set_registers_64!(self.inner.vcpufd, arr_reg_name_value).unwrap();
        }

        Ok(())
    }

    async fn handle_mmio_intercept(
        &self,
        message: &hv_message,
        devices: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let execution_state = message.to_memory_info().unwrap().header.execution_state;
        // SAFETY: This union only contains one field.
        let mmio_execution_state = unsafe { execution_state.__bindgen_anon_1 };
        let interruption_pending = mmio_execution_state.interruption_pending() != 0;

        self.emulate(message, devices, interruption_pending).await
    }

    fn handle_synic_deliverable_exit(&self, message: &hv_message, _devices: &impl CpuIo) {
        let info = message.to_sint_deliverable_info().unwrap();
        self.flush_messages(info.deliverable_sints);
    }

    fn handle_hypercall_intercept(&self, message: &hv_message, devices: &impl CpuIo) {
        let info = message.to_hypercall_intercept_info().unwrap();
        let execution_state = info.header.execution_state;
        // SAFETY: Accessing the raw field of this union is always safe.
        let vp_state = unsafe { HvX64VpExecutionState::from(execution_state.as_uint16) };
        let is_64bit = vp_state.cr0_pe() && vp_state.efer_lma();
        let mut hpc_context = MshvHypercallContext {
            rax: info.rax,
            rbx: info.rbx,
            rcx: info.rcx,
            rdx: info.rdx,
            r8: info.r8,
            rsi: info.rsi,
            rdi: info.rdi,
            xmm: info.xmmregisters,
        };
        let mut handler = MshvHypercallHandler {
            bus: devices,
            context: &mut hpc_context,
            rip: info.header.rip,
            rip_dirty: false,
            xmm_dirty: false,
            gp_dirty: false,
        };

        MshvHypercallHandler::DISPATCHER.dispatch(
            &self.partition.gm,
            X64RegisterIo::new(&mut handler, is_64bit),
        );

        let mut dirty_regs = ArrayVec::<hv_register_assoc, 14>::new();

        if handler.gp_dirty {
            dirty_regs.extend([
                hv_register_assoc {
                    name: mshv_bindings::hv_register_name_HV_X64_REGISTER_RAX,
                    value: hv_register_value {
                        reg64: handler.context.rax,
                    },
                    ..Default::default()
                },
                hv_register_assoc {
                    name: mshv_bindings::hv_register_name_HV_X64_REGISTER_RBX,
                    value: hv_register_value {
                        reg64: handler.context.rbx,
                    },
                    ..Default::default()
                },
                hv_register_assoc {
                    name: mshv_bindings::hv_register_name_HV_X64_REGISTER_RCX,
                    value: hv_register_value {
                        reg64: handler.context.rcx,
                    },
                    ..Default::default()
                },
                hv_register_assoc {
                    name: mshv_bindings::hv_register_name_HV_X64_REGISTER_RDX,
                    value: hv_register_value {
                        reg64: handler.context.rdx,
                    },
                    ..Default::default()
                },
                hv_register_assoc {
                    name: mshv_bindings::hv_register_name_HV_X64_REGISTER_R8,
                    value: hv_register_value {
                        reg64: handler.context.r8,
                    },
                    ..Default::default()
                },
                hv_register_assoc {
                    name: mshv_bindings::hv_register_name_HV_X64_REGISTER_RSI,
                    value: hv_register_value {
                        reg64: handler.context.rsi,
                    },
                    ..Default::default()
                },
                hv_register_assoc {
                    name: mshv_bindings::hv_register_name_HV_X64_REGISTER_RDI,
                    value: hv_register_value {
                        reg64: handler.context.rdi,
                    },
                    ..Default::default()
                },
            ]);
        }

        if handler.xmm_dirty {
            dirty_regs.extend((0..5).map(|i| hv_register_assoc {
                name: mshv_bindings::hv_register_name_HV_X64_REGISTER_XMM0 + i,
                value: hv_register_value {
                    reg128: handler.context.xmm[i as usize],
                },
                ..Default::default()
            }));
        }

        if handler.rip_dirty {
            dirty_regs.push(hv_register_assoc {
                name: mshv_bindings::hv_register_name_HV_X64_REGISTER_RIP,
                value: hv_register_value { reg64: handler.rip },
                ..Default::default()
            });
        }

        if !dirty_regs.is_empty() {
            self.inner
                .vcpufd
                .set_reg(&dirty_regs)
                .expect("RIP setting is not a fallable operation");
        }
    }

    fn flush_messages(&self, deliverable_sints: u16) {
        let nonempty_sints =
            self.inner
                .message_queues
                .post_pending_messages(deliverable_sints, |sint, message| {
                    match self.partition.vmfd.post_message_direct(
                        self.vpindex.index(),
                        sint,
                        message.as_bytes(),
                    ) {
                        Ok(()) => {
                            tracing::trace!(sint, "sint message posted successfully");
                            Ok(())
                        }
                        Err(e) => {
                            // TODO: handle errors appropriately
                            tracing::trace!(error = %e, "dropping sint message");
                            Err(HvError::ObjectInUse)
                        }
                    }
                });

        {
            // To avoid an additional get_reg hypercall, clear w/ deliverable sints mask
            let mut notifications = self.inner.deliverability_notifications.lock();
            let remaining_sints = notifications.sints() & !deliverable_sints;
            notifications.set_sints(remaining_sints);
        }

        if nonempty_sints != 0 {
            self.partition
                .request_sint_notifications(self.vpindex, nonempty_sints);
        }
    }

    fn emulation_cache(&self) -> MshvEmuCache {
        let regs = self.inner.vcpufd.get_regs().unwrap();
        let gps = [
            regs.rax, regs.rcx, regs.rdx, regs.rbx, regs.rsp, regs.rbp, regs.rsi, regs.rdi,
            regs.r8, regs.r9, regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
        ];
        let rip = regs.rip;
        let rflags = regs.rflags;

        let sregs = self.inner.vcpufd.get_sregs().unwrap();
        let segs = [
            x86emu_sreg_from_mshv_sreg(sregs.es),
            x86emu_sreg_from_mshv_sreg(sregs.cs),
            x86emu_sreg_from_mshv_sreg(sregs.ss),
            x86emu_sreg_from_mshv_sreg(sregs.ds),
            x86emu_sreg_from_mshv_sreg(sregs.fs),
            x86emu_sreg_from_mshv_sreg(sregs.gs),
        ];
        let cr0 = sregs.cr0;
        let efer = sregs.efer;

        MshvEmuCache {
            gps,
            segs,
            rip,
            rflags: rflags.into(),
            cr0,
            efer,
        }
    }
}

struct MshvEmulationState<'a> {
    partition: &'a MshvPartitionInner,
    processor: &'a MshvVpInner,
    vp_index: VpIndex,
    message: &'a hv_message,
    interruption_pending: bool,
    cache: MshvEmuCache,
}

impl EmulatorSupport for MshvEmulationState<'_> {
    fn vp_index(&self) -> VpIndex {
        self.vp_index
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.partition.caps.vendor
    }

    fn gp(&mut self, reg: x86emu::Gp) -> u64 {
        self.cache.gps[reg as usize]
    }

    fn set_gp(&mut self, reg: x86emu::Gp, v: u64) {
        self.cache.gps[reg as usize] = v;
    }

    fn rip(&mut self) -> u64 {
        self.cache.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.cache.rip = v;
    }

    fn segment(&mut self, reg: x86emu::Segment) -> SegmentRegister {
        self.cache.segs[reg as usize]
    }

    fn efer(&mut self) -> u64 {
        self.cache.efer
    }

    fn cr0(&mut self) -> u64 {
        self.cache.cr0
    }

    fn rflags(&mut self) -> RFlags {
        self.cache.rflags
    }

    fn set_rflags(&mut self, v: RFlags) {
        self.cache.rflags = v;
    }

    fn xmm(&mut self, reg: usize) -> u128 {
        assert!(reg < 16);
        let name = HvX64RegisterName(HvX64RegisterName::Xmm0.0 + reg as u32);
        // SAFETY: `HvRegisterAssoc` and `hv_register_assoc` have the same layout.
        let reg = unsafe {
            std::mem::transmute::<HvRegisterAssoc, hv_register_assoc>(HvRegisterAssoc::from((
                name, 0u128,
            )))
        };
        let _ = self.processor.vcpufd.get_reg(&mut [reg]);
        // SAFETY: Accessing the u128 field of this union is always safe.
        hvu128_to_u128(unsafe { &reg.value.reg128 })
    }

    fn set_xmm(&mut self, reg: usize, value: u128) {
        assert!(reg < 16);
        let name = HvX64RegisterName(HvX64RegisterName::Xmm0.0 + reg as u32);
        // SAFETY: `HvRegisterAssoc` and `hv_register_assoc` have the same layout.
        let reg = unsafe {
            std::mem::transmute::<HvRegisterAssoc, hv_register_assoc>(HvRegisterAssoc::from((
                name, value,
            )))
        };
        self.processor.vcpufd.set_reg(&[reg]).unwrap();
    }

    fn flush(&mut self) {
        let arr_reg_name_value = [
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RIP,
                self.cache.rip,
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RFLAGS,
                self.cache.rflags.into(),
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RAX,
                self.cache.gps[0],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RCX,
                self.cache.gps[1],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RDX,
                self.cache.gps[2],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RBX,
                self.cache.gps[3],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RSP,
                self.cache.gps[4],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RBP,
                self.cache.gps[5],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RSI,
                self.cache.gps[6],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_RDI,
                self.cache.gps[7],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_R8,
                self.cache.gps[8],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_R9,
                self.cache.gps[9],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_R10,
                self.cache.gps[10],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_R11,
                self.cache.gps[11],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_R12,
                self.cache.gps[12],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_R13,
                self.cache.gps[13],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_R14,
                self.cache.gps[14],
            ),
            (
                mshv_bindings::hv_register_name_HV_X64_REGISTER_R15,
                self.cache.gps[15],
            ),
        ];

        set_registers_64!(self.processor.vcpufd, arr_reg_name_value).unwrap();
    }

    fn instruction_bytes(&self) -> &[u8] {
        match HvMessageType(self.message.header.message_type) {
            HvMessageType::HvMessageTypeGpaIntercept
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeUnacceptedGpa => {
                // SAFETY: We have checked the message type.
                unsafe {
                    let info = (&raw const self.message.u.payload)
                        .cast::<hv_x64_memory_intercept_message>();
                    let instruction_bytes = &raw const (*info).instruction_bytes;
                    let instruction_byte_count =
                        std::ptr::read_unaligned(&raw const (*info).instruction_byte_count);
                    std::slice::from_raw_parts(
                        instruction_bytes.cast(),
                        instruction_byte_count as usize,
                    )
                }
            }
            HvMessageType::HvMessageTypeX64IoPortIntercept => {
                // SAFETY: We have checked the message type.
                unsafe {
                    let info = (&raw const self.message.u.payload)
                        .cast::<hv_x64_io_port_intercept_message>();
                    let instruction_bytes = &raw const (*info).instruction_bytes;
                    let instruction_byte_count =
                        std::ptr::read_unaligned(&raw const (*info).instruction_byte_count);
                    std::slice::from_raw_parts(
                        instruction_bytes.cast(),
                        instruction_byte_count as usize,
                    )
                }
            }
            _ => unreachable!(),
        }
    }

    fn physical_address(&self) -> Option<u64> {
        if self.message.header.message_type == HvMessageType::HvMessageTypeGpaIntercept.0
            || self.message.header.message_type == HvMessageType::HvMessageTypeUnmappedGpa.0
            || self.message.header.message_type == HvMessageType::HvMessageTypeUnacceptedGpa.0
        {
            let info = self.message.to_memory_info().unwrap();
            Some(info.guest_physical_address)
        } else {
            None
        }
    }

    fn initial_gva_translation(
        &mut self,
    ) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        if (self.message.header.message_type != HvMessageType::HvMessageTypeGpaIntercept.0)
            && (self.message.header.message_type != HvMessageType::HvMessageTypeUnmappedGpa.0)
            && (self.message.header.message_type != HvMessageType::HvMessageTypeUnacceptedGpa.0)
        {
            return None;
        }

        let message = self.message.to_memory_info().unwrap();

        // SAFETY: access to union as uint8 is safe because in this case, the actual
        // type doesn't matter so much as the bits
        let memory_access_info =
            unsafe { hvdef::HvX64MemoryAccessInfo::from(message.memory_access_info.as_uint8) };

        if !memory_access_info.gva_gpa_valid() {
            return None;
        }

        if let Ok(translate_mode) = TranslateMode::try_from(hvdef::HvInterceptAccessType(
            message.header.intercept_access_type,
        )) {
            Some(virt_support_x86emu::emulate::InitialTranslation {
                gva: message.guest_virtual_address,
                gpa: message.guest_physical_address,
                translate_mode,
            })
        } else {
            None
        }
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        _mode: TranslateMode,
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError> {
        // TODO: No VTL2 supported so always return Ok.
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
        emulate_translate_gva(self, gva, mode)
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        // SAFETY: `HvRegisterAssoc` and `hv_register_assoc` have the same layout.
        let reg = unsafe {
            &[
                std::mem::transmute::<HvRegisterAssoc, hv_register_assoc>(HvRegisterAssoc::from((
                    HvX64RegisterName::PendingEvent0,
                    u128::from(event_info.reg_0),
                ))),
                std::mem::transmute::<HvRegisterAssoc, hv_register_assoc>(HvRegisterAssoc::from((
                    HvX64RegisterName::PendingEvent1,
                    u128::from(event_info.reg_1),
                ))),
            ]
        };
        self.processor.vcpufd.set_reg(reg).unwrap();
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        self.partition
            .memory
            .lock()
            .ranges
            .iter()
            .flatten()
            .any(|range| {
                (range.guest_pfn..range.guest_pfn + range.size).contains(&gpa)
                    && (!write
                        || range.flags & set_bits!(u8, MSHV_SET_MEM_BIT_WRITABLE)
                            == set_bits!(u8, MSHV_SET_MEM_BIT_WRITABLE))
            })
    }

    fn lapic_base_address(&self) -> Option<u64> {
        None
    }

    fn lapic_read(&mut self, _address: u64, _data: &mut [u8]) {
        unreachable!()
    }

    fn lapic_write(&mut self, _address: u64, _data: &[u8]) {
        unreachable!()
    }
}

impl TranslateGvaSupport for MshvEmulationState<'_> {
    fn guest_memory(&self) -> &GuestMemory {
        &self.partition.gm
    }

    fn acquire_tlb_lock(&mut self) {
        // The hypervisor automatically acquires the TLB lock for exo partitions.
    }

    fn registers(&mut self) -> TranslationRegisters {
        let mut reg = [
            HvX64RegisterName::Cr0,
            HvX64RegisterName::Cr4,
            HvX64RegisterName::Efer,
            HvX64RegisterName::Cr3,
            HvX64RegisterName::Rflags,
            HvX64RegisterName::Ss,
        ]
        .map(|n| HvRegisterAssoc::from((n, 0u64)));

        // SAFETY: `HvRegisterAssoc` and `hv_register_assoc` have the same size.
        unsafe {
            self.processor
                .vcpufd
                .get_reg(std::mem::transmute::<
                    &mut [HvRegisterAssoc],
                    &mut [hv_register_assoc],
                >(&mut reg[..]))
                .unwrap();
        }

        let [cr0, cr4, efer, cr3, rflags, ss] = reg.map(|v| v.value);

        TranslationRegisters {
            cr0: cr0.as_u64(),
            cr4: cr4.as_u64(),
            efer: efer.as_u64(),
            cr3: cr3.as_u64(),
            rflags: rflags.as_u64(),
            ss: from_seg(ss.as_segment()),
            encryption_mode: virt_support_x86emu::translate::EncryptionMode::None,
        }
    }
}

// TODO: Chunk this up into smaller types.
#[derive(Error, Debug)]
pub enum Error {
    #[error("operation not supported")]
    NotSupported,
    #[error("create_vm failed")]
    CreateVMFailed,
    #[error("failed to initialize VM")]
    CreateVMInitFailed(#[source] anyhow::Error),
    #[error("failed to create VCPU")]
    CreateVcpu(#[source] MshvError),
    #[error("vtl2 not supported")]
    Vtl2NotSupported,
    #[error("isolation not supported")]
    IsolationNotSupported,
    #[error("failed to stat /dev/mshv")]
    AvailableCheck(#[source] io::Error),
    #[error("failed to open /dev/mshv")]
    OpenMshv(#[source] MshvError),
    #[error("register access error")]
    Register(#[source] MshvError),
    #[error("install instercept failed")]
    InstallIntercept(#[source] MshvError),
    #[error("host does not support required cpu capabilities")]
    Capabilities(virt::PartitionCapabilitiesError),
}

impl MshvPartitionInner {
    fn request_msi(&self, request: MsiRequest) {
        let (address, data) = request.as_x86();
        let control = request.hv_x86_interrupt_control();
        let mshv_req = InterruptRequest {
            interrupt_type: control.interrupt_type().0,
            apic_id: address.virt_destination().into(),
            vector: data.vector().into(),
            level_triggered: control.x86_level_triggered(),
            logical_destination_mode: control.x86_logical_destination_mode(),
            long_mode: false,
        };

        if let Err(err) = self.vmfd.request_virtual_interrupt(&mshv_req) {
            tracelimit::warn_ratelimited!(
                address = request.address,
                data = request.data,
                error = &err as &dyn std::error::Error,
                "failed to request msi"
            );
        }
    }
}

impl virt::irqcon::IoApicRouting for MshvPartitionInner {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        self.irq_routes.set_irq_route(irq, request)
    }

    fn assert_irq(&self, irq: u8) {
        self.irq_routes
            .assert_irq(irq, |request| self.request_msi(request))
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

        self.vmfd.map_user_memory(mem_region)?;
        state.ranges[slot_to_use] = Some(mem_region);
        Ok(())
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let mut state = self.memory.lock();
        let (slot, range) = state
            .ranges
            .iter_mut()
            .enumerate()
            .find(|(_, range)| {
                range.as_ref().map(|r| (r.guest_pfn, r.size)) == Some((addr >> HV_PAGE_SHIFT, size))
            })
            .expect("can only unmap existing ranges of exact size");

        self.vmfd.unmap_user_memory(range.unwrap())?;
        state.ranges[slot] = None;
        Ok(())
    }
}

// TODO: implementation
struct MshvDoorbellEntry;

impl MshvDoorbellEntry {
    pub fn new(
        _guest_address: u64,
        _value: Option<u64>,
        _length: Option<u32>,
        _fd: &Event,
    ) -> io::Result<MshvDoorbellEntry> {
        // TODO: implementation

        Ok(Self)
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
            guest_address,
            value,
            length,
            fd,
        )?))
    }
}

pub struct MshvHypercallContext {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub r8: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub xmm: [hv_u128; 6],
}

impl<T> hv1_hypercall::X64RegisterState for MshvHypercallHandler<'_, T> {
    fn rip(&mut self) -> u64 {
        self.rip
    }

    fn set_rip(&mut self, rip: u64) {
        self.rip = rip;
        self.rip_dirty = true;
    }

    fn gp(&mut self, n: hv1_hypercall::X64HypercallRegister) -> u64 {
        match n {
            hv1_hypercall::X64HypercallRegister::Rax => self.context.rax,
            hv1_hypercall::X64HypercallRegister::Rcx => self.context.rcx,
            hv1_hypercall::X64HypercallRegister::Rdx => self.context.rdx,
            hv1_hypercall::X64HypercallRegister::Rbx => self.context.rbx,
            hv1_hypercall::X64HypercallRegister::Rsi => self.context.rsi,
            hv1_hypercall::X64HypercallRegister::Rdi => self.context.rdi,
            hv1_hypercall::X64HypercallRegister::R8 => self.context.r8,
        }
    }

    fn set_gp(&mut self, n: hv1_hypercall::X64HypercallRegister, value: u64) {
        *match n {
            hv1_hypercall::X64HypercallRegister::Rax => &mut self.context.rax,
            hv1_hypercall::X64HypercallRegister::Rcx => &mut self.context.rcx,
            hv1_hypercall::X64HypercallRegister::Rdx => &mut self.context.rdx,
            hv1_hypercall::X64HypercallRegister::Rbx => &mut self.context.rbx,
            hv1_hypercall::X64HypercallRegister::Rsi => &mut self.context.rsi,
            hv1_hypercall::X64HypercallRegister::Rdi => &mut self.context.rdi,
            hv1_hypercall::X64HypercallRegister::R8 => &mut self.context.r8,
        } = value;
        self.gp_dirty = true;
    }

    fn xmm(&mut self, n: usize) -> u128 {
        let r = &self.context.xmm[n];
        hvu128_to_u128(r)
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        self.context.xmm[n] = u128_to_hvu128(value);
        self.xmm_dirty = true;
    }
}

fn hvu128_to_u128(r: &hv_u128) -> u128 {
    (r.high_part as u128) << 64 | r.low_part as u128
}

fn u128_to_hvu128(value: u128) -> hv_u128 {
    hv_u128 {
        high_part: (value >> 64) as u64,
        low_part: (value & (u64::MAX as u128)) as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u128_roundtrip() {
        let original = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;
        let hv = u128_to_hvu128(original);
        let roundtrip = hvu128_to_u128(&hv);
        assert_eq!(roundtrip, original);
    }
}

struct MshvHypercallHandler<'a, T> {
    bus: &'a T,
    context: &'a mut MshvHypercallContext,
    rip: u64,
    rip_dirty: bool,
    xmm_dirty: bool,
    gp_dirty: bool,
}

impl<T: CpuIo> MshvHypercallHandler<'_, T> {
    const DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [hv1_hypercall::HvPostMessage, hv1_hypercall::HvSignalEvent],
    );
}

impl<T: CpuIo> hv1_hypercall::PostMessage for MshvHypercallHandler<'_, T> {
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> hvdef::HvResult<()> {
        self.bus
            .post_synic_message(Vtl::Vtl0, connection_id, false, message)
    }
}

impl<T: CpuIo> hv1_hypercall::SignalEvent for MshvHypercallHandler<'_, T> {
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        self.bus.signal_synic_event(Vtl::Vtl0, connection_id, flag)
    }
}

impl Inspect for MshvPartition {
    fn inspect(&self, req: inspect::Request<'_>) {
        // TODO: implementation
        req.respond();
    }
}

impl InspectMut for MshvProcessor<'_> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
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
        Err(Error::NotSupported)
    }

    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        let vpinner = self.inner;
        let _cleaner = MshvVpInnerCleaner { vpinner };
        let vcpufd = &vpinner.vcpufd;

        // Ensure this thread is uniquely running the VP, and store the thread
        // ID to support cancellation.
        assert!(vpinner.thread.write().replace(Pthread::current()).is_none());

        loop {
            vpinner.needs_yield.maybe_yield().await;
            stop.check()?;

            match vcpufd.run() {
                Ok(exit) => match HvMessageType(exit.header.message_type) {
                    HvMessageType::HvMessageTypeUnrecoverableException => {
                        return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                    }
                    HvMessageType::HvMessageTypeX64IoPortIntercept => {
                        self.handle_io_port_intercept(&exit, dev).await?;
                    }
                    HvMessageType::HvMessageTypeUnmappedGpa
                    | HvMessageType::HvMessageTypeGpaIntercept => {
                        self.handle_mmio_intercept(&exit, dev).await?;
                    }
                    HvMessageType::HvMessageTypeSynicSintDeliverable => {
                        tracing::trace!("SYNIC_SINT_DELIVERABLE");
                        self.handle_synic_deliverable_exit(&exit, dev);
                    }
                    HvMessageType::HvMessageTypeHypercallIntercept => {
                        tracing::trace!("HYPERCALL_INTERCEPT");
                        self.handle_hypercall_intercept(&exit, dev);
                    }
                    exit => {
                        panic!("Unhandled vcpu exit code {exit:?}");
                    }
                },

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
}

fn x86emu_sreg_from_mshv_sreg(reg: mshv_bindings::SegmentRegister) -> SegmentRegister {
    let reg: hv_x64_segment_register = hv_x64_segment_register::from(reg);
    // SAFETY: This union only contains one field.
    let attributes: u16 = unsafe { reg.__bindgen_anon_1.attributes };

    SegmentRegister {
        base: reg.base,
        limit: reg.limit,
        selector: reg.selector,
        attributes: attributes.into(),
    }
}

fn from_seg(reg: hvdef::HvX64SegmentRegister) -> SegmentRegister {
    SegmentRegister {
        base: reg.base,
        limit: reg.limit,
        selector: reg.selector,
        attributes: reg.attributes.into(),
    }
}

impl virt::Synic for MshvPartition {
    fn post_message(&self, _vtl: Vtl, vp: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        self.inner
            .post_message(vp, sint, &HvMessage::new(HvMessageType(typ), 0, payload));
    }

    fn new_guest_event_port(
        &self,
        _vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort> {
        Box::new(MshvGuestEventPort {
            partition: Arc::downgrade(&self.inner),
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
                    .vmfd
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
