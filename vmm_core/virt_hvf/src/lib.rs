// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(all(target_os = "macos", guest_is_native, guest_arch = "aarch64"))]

//! A hypervisor backend using macos's Hypervisor framework.

// UNSAFETY: Calling Hypervisor framework APIs and manually managing memory.
#![expect(unsafe_code)]

mod abi;
mod hypercall;
mod vp_state;

use crate::hypercall::HvfHypercallHandler;
use aarch64defs::Cpsr64;
use aarch64defs::ExceptionClass;
use aarch64defs::IssDataAbort;
use aarch64defs::IssSystem;
use aarch64defs::MpidrEl1;
use aarch64defs::smccc::FastCall;
use aarch64defs::smccc::PsciError;
use aarch64defs::smccc::SmcCall;
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
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::task::Waker;
use std::task::ready;
use std::time::Duration;
use thiserror::Error;
use virt::BindProcessor;
use virt::NeedsYield;
use virt::Processor;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::aarch64::Aarch64PartitionCapabilities;
use virt::aarch64::vm::AccessVmState;
use virt::io::CpuIo;
use virt::state::StateElement;
use virt::vp::AccessVpState;
use virt_support_gic as gic;
use vm_topology::processor::aarch64::Aarch64VpInfo;
use vmcore::interrupt::Interrupt;
use vmcore::reference_time::GetReferenceTime;
use vmcore::reference_time::ReferenceTimeResult;
use vmcore::reference_time::ReferenceTimeSource;
use vmcore::synic::GuestEventPort;
use vmcore::vmtime::VmTimeAccess;

const PPI_VTIMER: u32 = 20;

const HV_ARM64_HVC_SMCCC_IDENTIFIER: u32 = (1 << 30) | (6 << 24) | 1;

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

        let mut gicd = gic::Distributor::new(
            self.config.processor_topology.gic_distributor_base(),
            MemoryRange::new(
                self.config.processor_topology.gic_redistributors_base()
                    ..self.config.processor_topology.gic_redistributors_base()
                        + aarch64defs::GIC_REDISTRIBUTOR_SIZE
                            * self.config.processor_topology.vp_count() as u64,
            ),
            256,
        );
        let gicrs = self
            .config
            .processor_topology
            .vps_arch()
            .map(|vp_info| gicd.add_redistributor(vp_info.mpidr.into(), true))
            .collect::<Vec<_>>();

        let inner = Arc::new(HvfPartitionInner {
            caps: Aarch64PartitionCapabilities {},
            vps: self
                .config
                .processor_topology
                .vps_arch()
                .map(|vp_info| HvfVpInner {
                    needs_yield: NeedsYield::new(),
                    vcpu: (!0).into(),
                    message_queues: hv1_emulator::message_queues::MessageQueues::new(),
                    waker: Default::default(),
                    vp_info,
                    cpu_on: Default::default(),
                })
                .collect(),
            gicd,
            guest_memory: config.guest_memory.clone(),
            vmtime: self.config.vmtime.access("hvf"),
            hv1,
            mappings: Default::default(),
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
                vp_index: vp.base.vp_index,
                state: Some(VpInitState {
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
    type Device = virt::aarch64::gic_software_device::GicSoftwareDevice;

    fn reference_time_source(&self) -> Option<ReferenceTimeSource> {
        Some(ReferenceTimeSource::from(
            self.inner.clone() as Arc<dyn GetReferenceTime>
        ))
    }

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        Some(self)
    }
}

impl virt::DeviceBuilder for HvfPartition {
    fn build(&self, _vtl: Vtl, _device_id: u64) -> Result<Self::Device, Self::Error> {
        Ok(virt::aarch64::gic_software_device::GicSoftwareDevice::new(
            self.inner.clone(),
        ))
    }
}

impl GetReferenceTime for HvfPartitionInner {
    fn now(&self) -> ReferenceTimeResult {
        ReferenceTimeResult {
            ref_time: self.vmtime.now().as_100ns(),
            system_time: None,
        }
    }
}

impl virt::irqcon::ControlGic for HvfPartitionInner {
    fn set_spi_irq(&self, irq_id: u32, high: bool) {
        if let Some(vp) = self.gicd.set_pending(irq_id, high) {
            if let Some(vp) = self.vps.get(vp as usize) {
                vp.wake();
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
                vp.wake();
            }
        }
    }

    fn new_guest_event_port(
        &self,
        _vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort> {
        Box::new(HvfEventPort {
            partition: Arc::downgrade(&self.inner),
            params: Arc::new(RwLock::new(HvfEventPortParams {
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

struct HvfEventPort {
    partition: Weak<HvfPartitionInner>,
    params: Arc<RwLock<HvfEventPortParams>>,
}

struct HvfEventPortParams {
    vp: VpIndex,
    sint: u8,
    flag: u16,
}

impl GuestEventPort for HvfEventPort {
    fn interrupt(&self) -> Interrupt {
        let partition = self.partition.clone();
        let params = self.params.clone();
        Interrupt::from_fn(move || {
            if let Some(partition) = partition.upgrade() {
                let params = params.read();
                let HvfEventPortParams { vp, sint, flag } = *params;
                let _ =
                    partition
                        .hv1
                        .synic
                        .signal_event(vp, sint, flag, &mut |vector, _auto_eoi| {
                            if partition.gicd.raise_ppi(vp, vector) {
                                tracing::debug!(vector, "ppi from event");
                                partition.vps[vp.index() as usize].wake();
                            }
                        });
            }
        })
    }

    fn set_target_vp(&mut self, vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
        self.params.write().vp = VpIndex::new(vp);
        Ok(())
    }
}

impl virt::PartitionMemoryMapper for HvfPartition {
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        assert_eq!(vtl, Vtl::Vtl0);
        self.inner.clone()
    }
}

impl virt::PartitionMemoryMap for HvfPartitionInner {
    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
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
    ) -> anyhow::Result<()> {
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
    type StateAccess<'a>
        = HvfPartitionStateAccess<'a>
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
    vp_info: Aarch64VpInfo,
    #[inspect(skip)]
    vcpu: AtomicU64,
    message_queues: hv1_emulator::message_queues::MessageQueues,
    #[inspect(skip)]
    waker: RwLock<Option<Waker>>,
    cpu_on: Mutex<Option<CpuOnState>>,
}

#[derive(Debug, Inspect)]
struct CpuOnState {
    pc: u64,
    x0: u64,
}

impl HvfVpInner {
    fn cancel_run(&self) {
        let vcpu: u64 = self.vcpu.load(Ordering::SeqCst);
        if vcpu != !0 {
            // SAFETY: `&vcpu` points to a list of vcpu IDs of length 1.
            unsafe { abi::hv_vcpus_exit(&vcpu, 1) }.chk().unwrap();
        }
    }

    fn wake(&self) {
        if let Some(waker) = &*self.waker.read() {
            waker.wake_by_ref();
        }
    }
}

pub struct HvfProcessorBinder {
    partition: Arc<HvfPartitionInner>,
    vp_index: VpIndex,
    state: Option<VpInitState>,
}

#[derive(Inspect)]
struct VpInitState {
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

        let state = self.state.take().unwrap();
        let inner = &self.partition.vps[self.vp_index.index() as usize];

        // Initialize configuration registers.
        // Set 40 bit physical address width.
        vcpu.set_sys_reg(abi::HvSysReg::ID_AA64MMFR0_EL1, 2)?;
        // Enable GICv3 system registers.
        vcpu.set_sys_reg(abi::HvSysReg::ID_AA64PFR0_EL1, 1 << 24)?;
        // Set the MPIDR.
        vcpu.set_sys_reg(abi::HvSysReg::MPIDR_EL1, inner.vp_info.mpidr.into())?;

        // Store the vcpu index in the partition.
        inner.vcpu.store(vcpu.vcpu, Ordering::Relaxed);

        let mut vp = HvfProcessor {
            partition: &self.partition,
            inner,
            vcpu,
            wfi: false,
            on: inner.vp_info.base.vp_index.is_bsp(),
            gicr: state.gicr,
            hv1: state.hv1,
            vmtime: state.vmtime,
        };

        // Set initial register state.
        let mut state = vp.access_state(Vtl::Vtl0);
        state
            .set_registers(&StateElement::at_reset(
                &self.partition.caps,
                &inner.vp_info,
            ))
            .unwrap();

        Ok(vp)
    }
}

#[derive(InspectMut)]
pub struct HvfProcessor<'a> {
    #[inspect(skip)]
    partition: &'a HvfPartitionInner,
    #[inspect(flatten)]
    inner: &'a HvfVpInner,
    gicr: gic::Redistributor,
    hv1: ProcessorSynic,
    vmtime: VmTimeAccess,
    #[inspect(flatten)]
    vcpu: HvfVcpu,
    wfi: bool,
    on: bool,
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

    fn cpsr(&self) -> Cpsr64 {
        let cpsr = Cpsr64::from(
            self.reg(abi::HvReg::CPSR)
                .expect("unrecoverable error getting CPSR"),
        );
        assert!(!cpsr.aa32(), "ARM32 not supported");
        cpsr
    }

    fn gp(&self, n: u8) -> u64 {
        if n < 31 {
            self.reg(abi::HvReg(abi::HvReg::X0.0 + n as u32))
                .expect("unrecoverable error getting GP")
        } else {
            let reg = if self.cpsr().sp() {
                abi::HvSysReg::SP_EL1
            } else {
                abi::HvSysReg::SP_EL0
            };
            self.sys_reg(reg).expect("unrecoverable error getting SP")
        }
    }

    fn set_gp(&mut self, n: u8, value: u64) {
        if n < 31 {
            self.set_reg(abi::HvReg(abi::HvReg::X0.0 + n as u32), value)
                .expect("unrecoverable failure to set GP")
        } else {
            let reg = if self.cpsr().sp() {
                abi::HvSysReg::SP_EL1
            } else {
                abi::HvSysReg::SP_EL0
            };
            self.set_sys_reg(reg, value)
                .expect("unrecoverable failure to set SP")
        }
    }

    fn pc(&self) -> u64 {
        self.reg(abi::HvReg::PC)
            .expect("unrecoverable error getting PC")
    }

    fn set_pc(&mut self, pc: u64) {
        self.set_reg(abi::HvReg::PC, pc)
            .expect("unrecoverable failure to set PC")
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
                self.hv1
                    .post_message(sint, message, &mut |vector, _auto_eoi| {
                        self.gicr.raise(vector)
                    })
            });
    }

    fn handle_smccc(&mut self, fc: FastCall) {
        match SmcCall(fc.with_hint(false).with_smc64(false)) {
            SmcCall::SMCCC_VERSION => {
                self.vcpu.set_gp(0, (1 << 16) | 1);
            }
            SmcCall::SMCCC_ARCH_FEATURES => {
                let feature_bits =
                    match SmcCall(FastCall::from(self.vcpu.gp(1) as u32).with_smc64(false)) {
                        SmcCall::SMCCC_ARCH_FEATURES => Some(0),
                        _ => None,
                    };
                self.vcpu.set_gp(0, feature_bits.unwrap_or(!0));
            }
            call => {
                tracelimit::warn_ratelimited!(?call, "ignoring unknown SMCCC call");
                self.vcpu.set_gp(0, !0);
            }
        }
    }

    fn handle_psci(&mut self, fc: FastCall) -> Result<(), VpHaltReason> {
        let mask = if fc.smc64() {
            u64::MAX
        } else {
            u32::MAX as u64
        };
        let r = match SmcCall(fc.with_smc64(false).with_hint(false)) {
            SmcCall::PSCI_VERSION => 1 << 16,
            SmcCall::PSCI_FEATURES => {
                let feature_bits =
                    match SmcCall(FastCall::from(self.vcpu.gp(1) as u32).with_smc64(false)) {
                        SmcCall::SMCCC_VERSION => Some(0),
                        SmcCall::CPU_SUSPEND => Some(0),
                        SmcCall::CPU_ON => Some(0),
                        SmcCall::CPU_OFF => Some(0),
                        SmcCall::AFFINITY_INFO => Some(0),
                        SmcCall::SYSTEM_OFF => Some(0),
                        SmcCall::SYSTEM_RESET => Some(0),
                        SmcCall::PSCI_FEATURES => Some(0),
                        _ => None,
                    };
                feature_bits.unwrap_or(PsciError::NOT_SUPPORTED.0)
            }
            SmcCall::CPU_SUSPEND => PsciError::INVALID_PARAMETERS.0,
            SmcCall::CPU_ON => {
                let target_cpu = self.vcpu.gp(1) & mask;
                let entry_point = self.vcpu.gp(2) & mask;
                let context_id = self.vcpu.gp(3) & mask;
                if let Some(vp) = self.partition.vps.iter().find(|vp| {
                    u64::from(vp.vp_info.mpidr) & u64::from(MpidrEl1::AFFINITY_MASK) == target_cpu
                }) {
                    let mut cpu_on = vp.cpu_on.lock();
                    if cpu_on.is_some() {
                        PsciError::ON_PENDING.0
                    } else {
                        // TODO check already on
                        *cpu_on = Some(CpuOnState {
                            pc: entry_point,
                            x0: context_id,
                        });
                        drop(cpu_on);
                        vp.wake();
                        PsciError::SUCCESS.0
                    }
                } else {
                    PsciError::INVALID_PARAMETERS.0
                }
            }
            SmcCall::CPU_OFF => PsciError::DENIED.0,
            SmcCall::AFFINITY_INFO => PsciError::INVALID_PARAMETERS.0,
            SmcCall::SYSTEM_RESET => return Err(VpHaltReason::Reset),
            SmcCall::SYSTEM_OFF => return Err(VpHaltReason::PowerOff),
            SmcCall::MIGRATE_INFO_TYPE => PsciError::NOT_SUPPORTED.0,
            call => {
                tracelimit::warn_ratelimited!(?call, "ignoring unknown PSCI32 call");
                PsciError::NOT_SUPPORTED.0
            }
        };
        self.vcpu.set_gp(0, r as u64);
        Ok(())
    }

    fn handle_vendor_hyp(&mut self, fc: FastCall) {
        match SmcCall(fc.with_hint(false).with_smc64(false)) {
            SmcCall::VENDOR_HYP_UID => {
                for (i, &v) in hvdef::VENDOR_HYP_UID_MS_HYPERVISOR.iter().enumerate() {
                    self.vcpu.set_gp(i as u8, v.into());
                }
            }
            call => {
                tracelimit::warn_ratelimited!(?call, "ignoring unknown VENDOR_HYP call");
                self.vcpu.set_gp(0, !0);
            }
        }
    }
}

impl<'p> Processor for HvfProcessor<'p> {
    type StateAccess<'a>
        = vp_state::HvfVpStateAccess<'a, 'p>
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        _state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <vp_state::HvfVpStateAccess<'_, 'p> as AccessVpState>::Error> {
        Ok(())
    }

    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        let vp_index = self.inner.vp_info.base.vp_index;
        let mut last_waker = None;
        loop {
            self.inner.needs_yield.maybe_yield().await;

            poll_fn(|cx| {
                loop {
                    stop.check()?;

                    if !last_waker
                        .as_ref()
                        .is_some_and(|waker| cx.waker().will_wake(waker))
                    {
                        last_waker = Some(cx.waker().clone());
                        self.inner.waker.write().clone_from(&last_waker);
                    }

                    if let Some(cpu_on) = self.inner.cpu_on.lock().take() {
                        if self.on {
                            todo!("block this");
                        } else {
                            tracing::debug!(x0 = cpu_on.x0, pc = cpu_on.pc, "cpu on");
                            self.vcpu.set_gp(0, cpu_on.x0);
                            self.vcpu.set_pc(cpu_on.pc);
                            self.on = true;
                        }
                    }

                    if !self.on {
                        break Poll::Pending;
                    }

                    self.hv1
                        .request_sint_readiness(self.inner.message_queues.pending_sints());

                    let ref_time_now = self.vmtime.now().as_100ns();
                    let (ready_sints, next_ref_time) =
                        self.hv1.scan(ref_time_now, &mut |ppi, _auto_eoi| {
                            tracing::debug!(ppi, "ppi from message");
                            self.gicr.raise(ppi);
                        });

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

                    if self.partition.gicd.irq_pending(&self.gicr) {
                        // SAFETY: no requirements.
                        unsafe {
                            abi::hv_vcpu_set_pending_interrupt(
                                self.vcpu.vcpu,
                                abi::HvInterruptType::IRQ,
                                true,
                            )
                        }
                        .chk()
                        .unwrap();
                        self.wfi = false;
                    }

                    if self.wfi {
                        self.vmtime.set_timeout_if_before(
                            self.vmtime.now().wrapping_add(Duration::from_millis(2)),
                        );
                        ready!(self.vmtime.poll_timeout(cx));
                        self.gicr.raise(PPI_VTIMER);
                        continue;
                    }

                    break Poll::Ready(Result::<_, VpHaltReason>::Ok(()));
                }
            })
            .await?;

            if !self.gicr.is_pending_or_active(PPI_VTIMER) {
                // SAFETY: no requirements.
                unsafe {
                    abi::hv_vcpu_set_vtimer_mask(self.vcpu.vcpu, false)
                        .chk()
                        .unwrap();
                }
            }

            // SAFETY: we are not concurrently accessing `exit`.
            unsafe { abi::hv_vcpu_run(self.vcpu.vcpu) }
                .chk()
                .map_err(|err| dev.fatal_error(err.into()))?;

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
                        let pc = vcpu.pc();
                        vcpu.set_pc(pc.wrapping_add(instr_len));
                    };
                    match ExceptionClass(exception.syndrome.ec()) {
                        ExceptionClass::DATA_ABORT_LOWER => {
                            let iss = IssDataAbort::from(exception.syndrome.iss());
                            if !iss.isv() {
                                return Err(dev.fatal_error(
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
                                    0..=30 => self.vcpu.gp(reg),
                                    31 => 0,
                                    _ => unreachable!(),
                                }
                                .to_ne_bytes();
                                if !self
                                    .partition
                                    .gicd
                                    .write(exception.physical_address, &data[..len])
                                {
                                    dev.write_mmio(
                                        vp_index,
                                        exception.physical_address,
                                        &data[..len],
                                    )
                                    .await;
                                }
                            } else if reg != 31 {
                                let mut data = [0; 8];
                                if !self
                                    .partition
                                    .gicd
                                    .read(exception.physical_address, &mut data[..len])
                                {
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
                                self.vcpu.set_gp(reg, data);
                            }
                            advance(&mut self.vcpu);
                        }
                        ExceptionClass::SYSTEM => {
                            let iss = IssSystem::from(exception.syndrome.iss());
                            let reg = iss.system_reg();
                            if iss.direction() {
                                let value = self
                                    .partition
                                    .gicd
                                    .read_sysreg(&mut self.gicr, reg)
                                    .unwrap_or_else(|| {
                                        tracing::warn!(
                                            ?reg,
                                            "returning zero for unknown system register"
                                        );
                                        0
                                    });
                                self.vcpu.set_gp(iss.rt(), value);
                            } else {
                                let value = self.vcpu.gp(iss.rt());
                                if !self.partition.gicd.write_sysreg(
                                    &mut self.gicr,
                                    reg,
                                    value,
                                    |index| self.partition.vps[index].wake(),
                                ) {
                                    tracing::warn!(
                                        ?reg,
                                        value,
                                        "ignoring write to unknown system register"
                                    );
                                }
                            }
                            advance(&mut self.vcpu);
                        }
                        ec @ (ExceptionClass::HVC | ExceptionClass::SMC) => {
                            // HVC automatically advances pc.
                            let mut advance_pc = ec == ExceptionClass::SMC;
                            match exception.syndrome.iss() as u16 {
                                0 => {
                                    let x0 = self.vcpu.gp(0) as u32;
                                    let fc = FastCall::from(x0);
                                    let handled = 'handle: {
                                        if fc.fast() {
                                            match fc.service() {
                                                aarch64defs::smccc::Service::SMCCC => {
                                                    self.handle_smccc(fc);
                                                }
                                                aarch64defs::smccc::Service::PSCI => {
                                                    self.handle_psci(fc)?
                                                }
                                                aarch64defs::smccc::Service::VENDOR_HYP => {
                                                    self.handle_vendor_hyp(fc);
                                                }
                                                _ => break 'handle false,
                                            }
                                        } else {
                                            match x0 {
                                                HV_ARM64_HVC_SMCCC_IDENTIFIER
                                                    if ec == ExceptionClass::HVC =>
                                                {
                                                    self.hypercall(dev, true);
                                                    advance_pc = false;
                                                }
                                                _ => break 'handle false,
                                            }
                                        }
                                        true
                                    };
                                    if !handled {
                                        tracing::warn!(x0, ?ec, "ignoring SMCCC HVC/SMC");
                                        // Set not supported error.
                                        self.vcpu.set_gp(0, !0);
                                    }
                                }
                                1 => self.hypercall(dev, false),
                                immed => {
                                    tracing::warn!(immed, ?ec, "ignoring HVC/SMC");
                                    self.vcpu.set_gp(0, !0);
                                }
                            }
                            if advance_pc {
                                advance(&mut self.vcpu);
                            }
                        }
                        ExceptionClass::WFI => {
                            self.wfi = true;
                            advance(&mut self.vcpu);
                        }
                        class => {
                            return Err(dev.fatal_error(
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
                    return Err(dev.fatal_error(
                        anyhow::anyhow!("unsupported exit reason: {reason:?}").into(),
                    ));
                }
            }
        }
    }

    fn flush_async_requests(&mut self) {}

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);
        vp_state::HvfVpStateAccess { processor: self }
    }
}
