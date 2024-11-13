// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A hypervisor backend using macos's Hypervisor framework.

// UNSAFETY: Calling Hypervisor framework APIs and manually managing memory.
#![allow(unsafe_code)]
#![cfg(all(target_os = "macos", target_arch = "aarch64"))] // xtask-fmt allow-target-arch sys-crate

mod abi;
mod hypercall;
mod vp_state;

use crate::hypercall::HvfHypercallHandler;
use aarch64defs::Cpsr64;
use aarch64defs::ExceptionClass;
use aarch64defs::IssDataAbort;
use aarch64defs::IssSystem;
use aarch64defs::SystemReg;
use abi::HvfError;
use anyhow::Context;
use guestmem::GuestMemory;
use hv1_emulator::synic::GlobalSynic;
use hv1_emulator::synic::ProcessorSynic;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::Vtl;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::convert::Infallible;
use std::future::poll_fn;
use std::ops::Deref;
use std::ops::Range;
use std::ptr::null_mut;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Weak;
use std::task::ready;
use std::time::Duration;
use thiserror::Error;
use virt::aarch64::vm::AccessVmState;
use virt::aarch64::Aarch64PartitionCapabilities;
use virt::io::CpuIo;
use virt::BindProcessor;
use virt::NeedsYield;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::VpInfo;
use virt_support_gic as gic;
use vmcore::interrupt::Interrupt;
use vmcore::synic::GuestEventPort;
use vmcore::vmtime::VmTimeAccess;

const PPI_VTIMER: u32 = 20;

const HV_ARM64_HVC_SMCCC_IDENTIFIER: u64 = (1 << 30) | (6 << 24) | 1;

#[derive(Debug)]
pub struct HvfHypervisor;

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] anyhow::Error);

impl From<HvfError> for Error {
    fn from(value: HvfError) -> Self {
        <Result<(), _>>::Err(value)
            .context("hypervisor framework error")
            .unwrap_err()
            .into()
    }
}

impl virt::Hypervisor for HvfHypervisor {
    type ProtoPartition<'a> = HvfProtoPartition<'a>;
    type Partition = HvfPartition;
    type Error = Error;

    fn is_available(&self) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn new_partition<'a>(
        &'a mut self,
        config: virt::ProtoPartitionConfig<'a>,
    ) -> Result<Self::ProtoPartition<'a>, Self::Error> {
        Ok(HvfProtoPartition { config })
    }
}

pub struct HvfProtoPartition<'a> {
    config: virt::ProtoPartitionConfig<'a>,
}

impl virt::ProtoPartition for HvfProtoPartition<'_> {
    type Partition = HvfPartition;
    type ProcessorBinder = HvfProcessorBinder;
    type Error = Error;

    fn build(
        self,
        config: virt::PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        // SAFETY: no safety requirements.
        unsafe { abi::hv_vm_create(null_mut()) }.chk()?;

        let hv1 = HvfHv1State::new(self.config.processor_topology.vp_count());
        let hv1_vps = self
            .config
            .processor_topology
            .vps()
            .map(|vp_info| hv1.synic.add_vp(vp_info.vp_index))
            .collect::<Vec<_>>();

        let mut gicd = gic::Distributor::new(256);
        let gicrs = self
            .config
            .processor_topology
            .vps()
            .map(|_| gicd.add_redistributor())
            .collect::<Vec<_>>();

        let inner = Arc::new(HvfPartitionInner {
            caps: Aarch64PartitionCapabilities {},
            vps: self
                .config
                .processor_topology
                .vps()
                .map(|_| HvfVpInner {
                    needs_yield: NeedsYield::new(),
                    vcpu: (!0).into(),
                    message_queues: hv1_emulator::message_queues::MessageQueues::new(),
                })
                .collect(),
            gicd,
            guest_memory: config.guest_memory.clone(),
            vmtime: self.config.vmtime.access("hvf"),
            hv1,
            mappings: Default::default(),
            gicd_range: self.config.processor_topology.gic_distributor_base()
                ..self.config.processor_topology.gic_distributor_base()
                    + aarch64defs::GIC_DISTRIBUTOR_SIZE,
        });

        let mut vps = Vec::new();
        for ((vp, hv1), gicr) in self
            .config
            .processor_topology
            .vps_arch()
            .zip(hv1_vps)
            .zip(gicrs)
        {
            vps.push(HvfProcessorBinder {
                partition: inner.clone(),
                state: Some(VpInitState {
                    info: vp.base,
                    gicr,
                    hv1,
                    vmtime: self
                        .config
                        .vmtime
                        .access(format!("vp{}", vp.base.vp_index.index())),
                    gicr_range: vp.gicr..vp.gicr + aarch64defs::GIC_REDISTRIBUTOR_SIZE,
                }),
            });
        }

        let partition = HvfPartition { inner };
        Ok((partition, vps))
    }

    fn max_physical_address_size(&self) -> u8 {
        // TODO
        40
    }
}

#[derive(Inspect)]
#[inspect(transparent)]
pub struct HvfPartition {
    inner: Arc<HvfPartitionInner>,
}

impl Drop for HvfPartitionInner {
    fn drop(&mut self) {
        // SAFETY: no safety requirements.
        unsafe { abi::hv_vm_destroy() }.chk().unwrap();
    }
}

impl virt::Partition for HvfPartition {
    fn supports_reset(
        &self,
    ) -> Option<&dyn virt::ResetPartition<Error = <Self as virt::Hv1>::Error>> {
        None
    }

    fn caps(&self) -> &Aarch64PartitionCapabilities {
        &self.inner.caps
    }

    fn request_msi(&self, _vtl: Vtl, _request: virt::irqcon::MsiRequest) {
        tracelimit::warn_ratelimited!("msis not supported");
    }

    fn request_yield(&self, vp_index: VpIndex) {
        let vp = &self.inner.vps[vp_index.index() as usize];
        if vp.needs_yield.request_yield() {
            vp.cancel_run();
        }
    }
}

impl virt::Aarch64Partition for HvfPartition {
    fn control_gic(&self, _vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic> {
        self.inner.clone()
    }
}

impl virt::Hv1 for HvfPartition {
    type Error = Error;
    type Device = virt::UnimplementedDevice;

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        None
    }
}

impl virt::irqcon::ControlGic for HvfPartitionInner {
    fn set_spi_irq(&self, irq_id: u32, high: bool) {
        if let Some(vp) = self.gicd.set_pending(irq_id, high) {
            if let Some(vp) = self.vps.get(vp as usize) {
                vp.cancel_run();
            }
        }
    }
}

impl virt::Synic for HvfPartition {
    fn post_message(&self, _vtl: Vtl, vp: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        if let Some(vp) = self.inner.vps.get(vp.index() as usize) {
            if vp
                .message_queues
                .enqueue_message(sint, &HvMessage::new(HvMessageType(typ), 0, payload))
            {
                vp.cancel_run();
            }
        }
    }

    fn new_guest_event_port(&self) -> Box<dyn GuestEventPort> {
        Box::new(HvfEventPort {
            partition: Arc::downgrade(&self.inner),
            params: Default::default(),
        })
    }

    fn prefer_os_events(&self) -> bool {
        false
    }
}

struct HvfEventPort {
    partition: Weak<HvfPartitionInner>,
    params: Arc<RwLock<Option<(VpIndex, u8, u16)>>>,
}

impl GuestEventPort for HvfEventPort {
    fn interrupt(&self) -> Interrupt {
        let partition = self.partition.clone();
        let params = self.params.clone();
        Interrupt::from_fn(move || {
            if let Some(partition) = partition.upgrade() {
                let params = params.read();
                if let Some((vp, sint, flag)) = *params {
                    let _ = partition.hv1.synic.signal_event(
                        &partition.guest_memory,
                        vp,
                        sint,
                        flag,
                        &mut |vector, _auto_eoi| {
                            if partition.gicd.raise_ppi(vp, vector) {
                                tracing::debug!(vector, "ppi from event");
                                partition.vps[vp.index() as usize].cancel_run();
                            }
                        },
                    );
                }
            }
        })
    }

    fn clear(&mut self) {
        *self.params.write() = None;
    }

    fn set(&mut self, _vtl: Vtl, vp: u32, sint: u8, flag: u16) {
        *self.params.write() = Some((VpIndex::new(vp), sint, flag));
    }
}

impl virt::PartitionMemoryMapper for HvfPartition {
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        assert_eq!(vtl, Vtl::Vtl0);
        self.inner.clone()
    }
}

impl virt::PartitionMemoryMap for HvfPartitionInner {
    fn unmap_range(&self, addr: u64, size: u64) -> Result<(), virt::Error> {
        let range = MemoryRange::new(addr..addr + size);
        self.mappings.lock().retain(|mapping| {
            if !range.overlaps(mapping) {
                return true;
            }
            assert!(range.contains(mapping));
            // SAFETY: no safety requirements.
            unsafe { abi::hv_vm_unmap(mapping.start(), mapping.len() as usize) }
                .chk()
                .expect("cannot fail");
            false
        });
        Ok(())
    }

    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> Result<(), virt::Error> {
        let mut mappings = self.mappings.lock();
        let mut flags = abi::HvMemoryFlags::READ.0;
        if writable {
            flags |= abi::HvMemoryFlags::WRITE.0;
        }
        if exec {
            flags |= abi::HvMemoryFlags::EXEC.0;
        }
        // SAFETY: the caller guarantees that the memory pointed to by data is
        // valid until `unmap_range` is called (or the partition is destroyed).
        unsafe { abi::hv_vm_map(data.cast(), addr, size, flags) }.chk()?;
        mappings.push(MemoryRange::new(addr..addr + size as u64));
        Ok(())
    }
}

impl virt::PartitionAccessState for HvfPartition {
    type StateAccess<'a> = HvfPartitionStateAccess<'a>
    where
        Self: 'a;

    fn access_state(&self, _vtl: Vtl) -> Self::StateAccess<'_> {
        HvfPartitionStateAccess {
            partition: &self.inner,
        }
    }
}

pub struct HvfPartitionStateAccess<'a> {
    partition: &'a HvfPartitionInner,
}

impl AccessVmState for HvfPartitionStateAccess<'_> {
    type Error = Error;

    fn caps(&self) -> &Aarch64PartitionCapabilities {
        &self.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Inspect)]
struct HvfPartitionInner {
    caps: Aarch64PartitionCapabilities,
    #[inspect(skip)]
    vps: Vec<HvfVpInner>,
    gicd: gic::Distributor,
    guest_memory: GuestMemory,
    vmtime: VmTimeAccess,
    hv1: HvfHv1State,
    #[inspect(with = "|x| inspect::adhoc(|req| inspect::iter_by_index(&*x.lock()).inspect(req))")]
    mappings: Mutex<Vec<MemoryRange>>,
    #[inspect(debug)]
    gicd_range: Range<u64>,
}

#[derive(Inspect)]
struct HvfHv1State {
    guest_os_id: AtomicU64,
    synic: GlobalSynic,
}

impl HvfHv1State {
    fn new(max_vp_count: u32) -> Self {
        Self {
            guest_os_id: 0.into(),
            synic: GlobalSynic::new(max_vp_count),
        }
    }
}

#[derive(Debug, Inspect)]
struct HvfVpInner {
    #[inspect(skip)]
    needs_yield: NeedsYield,
    #[inspect(skip)]
    vcpu: AtomicU64,
    message_queues: hv1_emulator::message_queues::MessageQueues,
}

impl HvfVpInner {
    fn cancel_run(&self) {
        let vcpu: u64 = self.vcpu.load(Ordering::SeqCst);
        if vcpu != !0 {
            // SAFETY: `&vcpu` points to a list of vcpu IDs of length 1.
            unsafe { abi::hv_vcpus_exit(&vcpu, 1) }.chk().unwrap();
        }
    }
}

pub struct HvfProcessorBinder {
    partition: Arc<HvfPartitionInner>,
    state: Option<VpInitState>,
}

#[derive(Inspect)]
struct VpInitState {
    info: VpInfo,
    gicr: gic::Redistributor,
    hv1: ProcessorSynic,
    vmtime: VmTimeAccess,
    #[inspect(debug)]
    gicr_range: Range<u64>,
}

impl BindProcessor for HvfProcessorBinder {
    type Processor<'a> = HvfProcessor<'a>;
    type Error = Error;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        let mut vcpu = HvfVcpu::new()?;

        // Initialize configuration registers.
        // Set 40 bit physical address width.
        vcpu.set_sys_reg(abi::HvSysReg::ID_AA64MMFR0_EL1, 2)?;
        // Enable GICv3 system registers.
        vcpu.set_sys_reg(abi::HvSysReg::ID_AA64PFR0_EL1, 1 << 24)?;

        let state = self.state.take().unwrap();
        let inner = &self.partition.vps[state.info.vp_index.index() as usize];

        // Store the vcpu index in the partition.
        inner.vcpu.store(vcpu.vcpu, Ordering::Relaxed);

        let vp = HvfProcessor {
            info: state.info,
            partition: &self.partition,
            inner,
            vcpu,
            wfi: false,
            gicr: state.gicr,
            hv1: state.hv1,
            vmtime: state.vmtime,
            gicr_range: state.gicr_range,
        };

        Ok(vp)
    }
}

#[derive(InspectMut)]
pub struct HvfProcessor<'a> {
    #[inspect(skip)]
    partition: &'a HvfPartitionInner,
    info: VpInfo,
    #[inspect(flatten)]
    inner: &'a HvfVpInner,
    gicr: gic::Redistributor,
    hv1: ProcessorSynic,
    vmtime: VmTimeAccess,
    #[inspect(flatten)]
    vcpu: HvfVcpu,
    wfi: bool,
    #[inspect(debug)]
    gicr_range: Range<u64>,
}

#[derive(Debug, Inspect)]
struct HvfVcpu {
    vcpu: u64,
    #[inspect(skip)]
    exit: ExitPtr,
}

#[derive(Debug)]
struct ExitPtr(*mut abi::HvVcpuExit);

impl Deref for ExitPtr {
    type Target = abi::HvVcpuExit;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the data pointed to is known to be valid and in fact
        // exclusively owned by us at this point.
        unsafe { &*self.0 }
    }
}

impl HvfVcpu {
    fn new() -> Result<Self, HvfError> {
        let mut vcpu = 0;
        let mut exit = null_mut();
        // SAFETY: `vcpu` and `exit` are valid buffers to receive the output parameters.
        unsafe { abi::hv_vcpu_create(&mut vcpu, &mut exit, null_mut()) }.chk()?;
        Ok(Self {
            vcpu,
            exit: ExitPtr(exit),
        })
    }

    fn gp(&self, n: u8) -> Result<u64, HvfError> {
        if n < 31 {
            self.reg(abi::HvReg(abi::HvReg::X0.0 + n as u32))
        } else {
            let cpsr = Cpsr64::from(self.reg(abi::HvReg::CPSR)?);
            assert!(!cpsr.aa32());
            let reg = if cpsr.sp() {
                abi::HvSysReg::SP_EL1
            } else {
                abi::HvSysReg::SP_EL0
            };
            self.sys_reg(reg)
        }
    }

    fn set_gp(&mut self, n: u8, value: u64) -> Result<(), HvfError> {
        if n < 31 {
            self.set_reg(abi::HvReg(abi::HvReg::X0.0 + n as u32), value)
        } else {
            let cpsr = Cpsr64::from(self.reg(abi::HvReg::CPSR)?);
            assert!(!cpsr.aa32());
            let reg = if cpsr.sp() {
                abi::HvSysReg::SP_EL1
            } else {
                abi::HvSysReg::SP_EL0
            };
            self.set_sys_reg(reg, value)
        }
    }

    fn reg(&self, reg: abi::HvReg) -> Result<u64, HvfError> {
        let mut value = 0;
        // SAFETY: `value` is a valid buffer to receive the output.
        unsafe {
            abi::hv_vcpu_get_reg(self.vcpu, reg, &mut value).chk()?;
        }
        Ok(value)
    }

    fn sys_reg(&self, reg: abi::HvSysReg) -> Result<u64, HvfError> {
        let mut value = 0;
        // SAFETY: `value` is a valid buffer to receive the output.
        unsafe {
            abi::hv_vcpu_get_sys_reg(self.vcpu, reg, &mut value).chk()?;
        }
        Ok(value)
    }

    fn set_reg(&mut self, reg: abi::HvReg, value: u64) -> Result<(), HvfError> {
        // SAFETY: no special rquirements
        unsafe {
            abi::hv_vcpu_set_reg(self.vcpu, reg, value).chk()?;
        }
        Ok(())
    }

    fn set_sys_reg(&mut self, reg: abi::HvSysReg, value: u64) -> Result<(), HvfError> {
        // SAFETY: no special rquirements
        unsafe {
            abi::hv_vcpu_set_sys_reg(self.vcpu, reg, value).chk()?;
        }
        Ok(())
    }
}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        // SAFETY: no special requirements
        unsafe { abi::hv_vcpu_destroy(self.vcpu) }
            .chk()
            .expect("vcpu destroy cannot fail");
    }
}

impl HvfProcessor<'_> {
    fn hypercall(&mut self, dev: &impl CpuIo, smccc: bool) {
        let guest_memory = &self.partition.guest_memory;
        let handler = HvfHypercallHandler::new(self, dev);
        HvfHypercallHandler::DISPATCHER.dispatch(
            guest_memory,
            hv1_hypercall::Arm64RegisterIo::new(handler, true, smccc),
        );
    }

    fn deliver_sints(&mut self, sints: u16) {
        self.inner
            .message_queues
            .post_pending_messages(sints, |sint, message| {
                self.hv1.post_message(
                    &self.partition.guest_memory,
                    sint,
                    message,
                    &mut |vector, _auto_eoi| self.gicr.raise(vector),
                )
            });
    }
}

impl<'p> virt::Processor for HvfProcessor<'p> {
    type Error = Error;
    type RunVpError = Error;

    type StateAccess<'a> = vp_state::HvfVpStateAccess<'a, 'p>
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        _state: Option<&virt::x86::DebugState>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn run_vp(
        &mut self,
        mut stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason<Error>> {
        loop {
            self.inner.needs_yield.maybe_yield().await;

            stop.check()?;

            let vp_index = self.info.vp_index;

            self.hv1
                .request_sint_readiness(self.inner.message_queues.pending_sints());

            let ref_time_now = self.vmtime.now().as_100ns();
            let (ready_sints, next_ref_time) = self.hv1.scan(
                ref_time_now,
                &self.partition.guest_memory,
                &mut |ppi, _auto_eoi| {
                    tracing::debug!(ppi, "ppi from message");
                    self.gicr.raise(ppi);
                },
            );

            if let Some(next_ref_time) = next_ref_time {
                // Convert from reference timer basis to vmtime basis via
                // difference of programmed timer and current reference time.
                const NUM_100NS_IN_SEC: u64 = 10 * 1000 * 1000;
                let ref_diff = next_ref_time.saturating_sub(ref_time_now);
                let ref_duration = Duration::new(
                    ref_diff / NUM_100NS_IN_SEC,
                    (ref_diff % NUM_100NS_IN_SEC) as u32 * 100,
                );
                let timeout = self.vmtime.now().wrapping_add(ref_duration);
                self.vmtime.set_timeout_if_before(timeout);
            }

            if ready_sints != 0 {
                self.deliver_sints(ready_sints);
                continue;
            }

            if self.gicr.irq_pending() || self.partition.gicd.irq_pending() {
                // SAFETY: no requirements.
                unsafe {
                    abi::hv_vcpu_set_pending_interrupt(
                        self.vcpu.vcpu,
                        abi::HvInterruptType::IRQ,
                        true,
                    )
                }
                .chk()
                .map_err(|err| VpHaltReason::Hypervisor(err.into()))?;
                self.wfi = false;
            }

            if self.wfi {
                self.vmtime.set_timeout_if_before(
                    self.vmtime.now().wrapping_add(Duration::from_millis(2)),
                );
                let timeout = poll_fn(|cx| {
                    ready!(self.vmtime.poll_timeout(cx));
                    self.gicr.raise(PPI_VTIMER);
                    ().into()
                });
                stop.until_stop(timeout).await?;
                continue;
            }

            if !self.gicr.is_pending_or_active(PPI_VTIMER) {
                // SAFETY: no requirements.
                unsafe {
                    abi::hv_vcpu_set_vtimer_mask(self.vcpu.vcpu, false)
                        .chk()
                        .map_err(|err| VpHaltReason::Hypervisor(err.into()))?;
                }
            }

            // SAFETY: we are not concurrently accessing `exit`.
            unsafe { abi::hv_vcpu_run(self.vcpu.vcpu) }
                .chk()
                .map_err(|err| VpHaltReason::Hypervisor(err.into()))?;

            match self.vcpu.exit.reason {
                abi::HvExitReason::CANCELED => {
                    continue;
                }
                abi::HvExitReason::EXCEPTION => {
                    let exception = self.vcpu.exit.exception;
                    tracing::trace!(
                        esr = u64::from(exception.syndrome),
                        va = exception.virtual_address,
                        pa = exception.physical_address,
                        "exception"
                    );
                    let advance = |vcpu: &mut HvfVcpu| {
                        let instr_len = if exception.syndrome.il() { 4 } else { 2 };
                        let pc = vcpu.reg(abi::HvReg::PC).expect("BUGBUG");
                        vcpu.set_reg(abi::HvReg::PC, pc.wrapping_add(instr_len))
                            .expect("BUGBUG");
                    };
                    match ExceptionClass(exception.syndrome.ec()) {
                        ExceptionClass::DATA_ABORT_LOWER => {
                            let iss = IssDataAbort::from(exception.syndrome.iss());
                            if !iss.isv() {
                                return Err(VpHaltReason::EmulationFailure(
                                    anyhow::anyhow!("can't handle data abort without isv: {iss:?}")
                                        .into(),
                                ));
                            }
                            let len = 1 << iss.sas();
                            let sign_extend = iss.sse();

                            // Per "AArch64 System Register Descriptions/D23.2 General system control registers"
                            // the SRT field is defined as
                            //
                            // > The register number of the Wt/Xt/Rt operand of the faulting
                            // > instruction.
                            //
                            // In the A64 ISA TRM, Wt/Xt/Rt is used to designate the register number where the SP
                            // register is not used whereas the addition of `|SP` tells that the SP register might
                            // be used. Hence, the SRT field uses `0b11111` to encode `xzr`.
                            //
                            // Writing to `xzr` has no arch-observable effects, reading returns the all-zero's bit
                            // pattern.
                            let reg = iss.srt();

                            if iss.wnr() {
                                let data = match reg {
                                    0..=30 => self.vcpu.gp(reg).expect("BUGBUG"),
                                    31 => 0,
                                    _ => unreachable!(),
                                }
                                .to_ne_bytes();
                                if self
                                    .partition
                                    .gicd_range
                                    .contains(&exception.physical_address)
                                {
                                    self.partition
                                        .gicd
                                        .write(exception.physical_address, &data[..len]);
                                } else if self.gicr_range.contains(&exception.physical_address) {
                                    self.gicr.write(
                                        exception.physical_address - self.gicr_range.start,
                                        &data[..len],
                                    );
                                } else {
                                    dev.write_mmio(
                                        vp_index,
                                        exception.physical_address,
                                        &data[..len],
                                    )
                                    .await;
                                }
                            } else if reg != 31 {
                                let mut data = [0; 8];
                                if self
                                    .partition
                                    .gicd_range
                                    .contains(&exception.physical_address)
                                {
                                    self.partition
                                        .gicd
                                        .read(exception.physical_address, &mut data[..len]);
                                } else if self.gicr_range.contains(&exception.physical_address) {
                                    self.gicr.read(
                                        exception.physical_address - self.gicr_range.start,
                                        &mut data[..len],
                                    );
                                } else {
                                    dev.read_mmio(
                                        vp_index,
                                        exception.physical_address,
                                        &mut data[..len],
                                    )
                                    .await;
                                }
                                let mut data = u64::from_ne_bytes(data);
                                if sign_extend {
                                    let shift = 64 - len * 8;
                                    data = ((data as i64) << shift >> shift) as u64;
                                    if !iss.sf() {
                                        data &= 0xffffffff;
                                    }
                                }
                                self.vcpu.set_gp(reg, data).expect("BUGBUG");
                            }
                            advance(&mut self.vcpu);
                        }
                        ExceptionClass::SYSTEM => {
                            let iss = IssSystem::from(exception.syndrome.iss());
                            if iss.direction() {
                                let value = match iss.system_reg() {
                                    SystemReg::ICC_IAR1_EL1 => {
                                        let mut intid = self.gicr.ack_group1();
                                        if intid == 1023 {
                                            intid = self.partition.gicd.ack();
                                        }
                                        intid.into()
                                    }
                                    reg => {
                                        tracing::warn!(
                                            ?reg,
                                            "returning zero for unknown system register"
                                        );
                                        0
                                    }
                                };
                                self.vcpu.set_gp(iss.rt(), value).expect("BUGBUG");
                            } else {
                                let value = self.vcpu.gp(iss.rt()).expect("BUGBUG");
                                match iss.system_reg() {
                                    SystemReg::ICC_EOIR1_EL1 => {
                                        let intid = value as u32;
                                        if intid < 32 {
                                            self.gicr.eoi_group1(intid);
                                        } else {
                                            self.partition.gicd.eoi(intid);
                                        }
                                    }
                                    reg => {
                                        tracing::warn!(
                                            ?reg,
                                            value,
                                            "ignoring write to unknown system register"
                                        );
                                    }
                                }
                            }
                            advance(&mut self.vcpu);
                        }
                        ExceptionClass::HVC => {
                            match exception.syndrome.iss() as u16 {
                                0 => {
                                    let x0 = self.vcpu.gp(0).expect("BUGBUG");
                                    match x0 {
                                        HV_ARM64_HVC_SMCCC_IDENTIFIER => self.hypercall(dev, true),
                                        _ => {
                                            let x0 = self.vcpu.gp(0).expect("BUGBUG");
                                            tracing::warn!(x0, "ignoring SMCCC HVC");
                                            // Set not supported error.
                                            self.vcpu.set_gp(0, !0).expect("BUGBUG");
                                        }
                                    }
                                }
                                1 => self.hypercall(dev, false),
                                immed => {
                                    tracing::warn!(immed, "ignoring HVC");
                                    self.vcpu.set_gp(0, !0).expect("BUGBUG");
                                    // HVC automatically advances pc.
                                }
                            }
                        }
                        ExceptionClass::SMC => {
                            let x0 = self.vcpu.gp(0).expect("BUGBUG");
                            tracing::warn!(immed = exception.syndrome.iss(), x0, "ignoring SMC");
                            // Set not supported error.
                            self.vcpu.set_gp(0, !0).expect("BUGBUG");
                            advance(&mut self.vcpu);
                        }
                        ExceptionClass::WFI => {
                            self.wfi = true;
                            advance(&mut self.vcpu);
                        }
                        class => {
                            return Err(VpHaltReason::Hypervisor(
                                anyhow::anyhow!(
                                    "unsupported exception class: {class:?} {iss:#x}",
                                    iss = exception.syndrome.iss()
                                )
                                .into(),
                            ));
                        }
                    }
                }
                abi::HvExitReason::VTIMER_ACTIVATED => {
                    self.gicr.raise(PPI_VTIMER);
                }
                reason => {
                    return Err(VpHaltReason::Hypervisor(
                        anyhow::anyhow!("unsupported exit reason: {reason:?}").into(),
                    ));
                }
            }
        }
    }

    fn flush_async_requests(&mut self) -> Result<(), Self::RunVpError> {
        Ok(())
    }

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);
        vp_state::HvfVpStateAccess { processor: self }
    }
}
