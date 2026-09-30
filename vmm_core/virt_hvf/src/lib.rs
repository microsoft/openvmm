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
use aarch64defs::DebugFeatures0El1;
use aarch64defs::ExceptionClass;
use aarch64defs::GicCpuInterface;
use aarch64defs::IntermPhysAddrSize;
use aarch64defs::IssDataAbort;
use aarch64defs::IssSystem;
use aarch64defs::MmFeatures0El1;
use aarch64defs::MmFeatures2El1;
use aarch64defs::MpidrEl1;
use aarch64defs::ProcessorFeatures0El1;
use aarch64defs::ProcessorFeatures1El1;
use aarch64defs::SystemReg;
use aarch64defs::Vendor;
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
use std::num::NonZeroU64;
use std::ops::Deref;
use std::ops::Range;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::task::Waker;
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
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeAccess;

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

    fn platform_info(&self) -> virt::PlatformInfo {
        virt::PlatformInfo {
            platform_gsiv: None,
            supports_gic_v3: true,
            supports_its: false,
            device_assignment_msi_iova: virt::DeviceAssignmentMsiIova::Unsupported,
        }
    }

    fn new_partition<'a>(
        &'a mut self,
        config: virt::ProtoPartitionConfig<'a>,
    ) -> Result<Self::ProtoPartition<'a>, Self::Error> {
        if config.isolation.is_isolated() {
            return Err(anyhow::anyhow!("HVF does not support isolated partitions").into());
        }

        let mut ipa_bit_length = 0;
        // SAFETY: `ipa_bit_length` is a valid out parameter.
        unsafe { abi::hv_vm_config_get_default_ipa_size(&mut ipa_bit_length) }
            .chk()
            .context("failed to query the default HVF IPA size")?;
        let ipa_bit_length =
            u8::try_from(ipa_bit_length).context("default HVF IPA size does not fit in u8")?;
        let ipa_range = IntermPhysAddrSize::from_ipa_bit_length(ipa_bit_length)
            .with_context(|| format!("unsupported default HVF IPA size: {ipa_bit_length}"))?;

        Ok(HvfProtoPartition {
            config,
            ipa_bit_length,
            ipa_range,
        })
    }
}

pub struct HvfProtoPartition<'a> {
    config: virt::ProtoPartitionConfig<'a>,
    ipa_bit_length: u8,
    ipa_range: IntermPhysAddrSize,
}

impl virt::ProtoPartition for HvfProtoPartition<'_> {
    type Partition = HvfPartition;
    type ProcessorBinder = HvfProcessorBinder;
    type Error = Error;

    fn build(
        self,
        config: virt::PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        use vm_topology::processor::aarch64::GicVersion;

        let gic_redistributors_base = match self.config.processor_topology.gic_version() {
            GicVersion::V3 {
                redistributors_base,
            } => redistributors_base,
            GicVersion::V2 { .. } => {
                return Err(
                    anyhow::anyhow!("HVF does not support GICv2; only GICv3 is supported").into(),
                );
            }
        };

        // SAFETY: no safety requirements. A null configuration selects the
        // default IPA width queried when the prototype partition was created.
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
                gic_redistributors_base
                    ..gic_redistributors_base
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
            caps: Aarch64PartitionCapabilities {
                isolation: virt::IsolationType::None,
                // Apple Silicon does not support aarch32.
                supports_aarch32_el0: false,
                vendor: Vendor::ARM,
            },
            virt_timer_ppi: self.config.processor_topology.virt_timer_ppi(),
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
            synic_ports: Default::default(),
            ipa_range: self.ipa_range,
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
                    gicr_range: {
                        // Guaranteed to be Some since we validated GICv3 above.
                        let gicr = vp.gicr.unwrap();
                        gicr..gicr + aarch64defs::GIC_REDISTRIBUTOR_SIZE
                    },
                }),
            });
        }

        let synic_ports = Arc::new(virt::synic::SynicPorts::new(inner.clone()));

        let partition = HvfPartition { inner, synic_ports };
        Ok((partition, vps))
    }

    fn max_physical_address_size(&self) -> u8 {
        self.ipa_bit_length
    }
}

#[derive(Inspect)]
#[inspect(transparent)]
pub struct HvfPartition {
    inner: Arc<HvfPartitionInner>,
    #[inspect(skip)]
    synic_ports: Arc<virt::synic::SynicPorts<HvfPartitionInner>>,
}

impl Drop for HvfPartitionInner {
    fn drop(&mut self) {
        // SAFETY: no safety requirements.
        if let Err(err) = unsafe { abi::hv_vm_destroy() }.chk() {
            tracing::error!(?err, "failed to destroy HVF VM");
        }
    }
}

impl virt::Partition for HvfPartition {
    fn initial_vp_state_source(&self) -> virt::InitialVpStateSource {
        virt::InitialVpStateSource::Registers
    }

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

    fn synic(&self) -> anyhow::Result<Arc<dyn vmcore::synic::SynicPortAccess>> {
        Ok(self.synic_ports.clone())
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

impl virt::synic::Synic for HvfPartitionInner {
    fn port_map(&self) -> &virt::synic::SynicPortMap {
        &self.synic_ports
    }

    fn post_message(&self, _vtl: Vtl, vp: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        if let Some(vp) = self.vps.get(vp.index() as usize) {
            if vp
                .message_queues
                .enqueue_message(sint, &HvMessage::new(HvMessageType(typ), 0, payload))
            {
                vp.wake();
            }
        }
    }

    fn new_guest_event_port(
        self: Arc<Self>,
        _vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort> {
        Box::new(HvfEventPort {
            partition: Arc::downgrade(&self),
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
    virt_timer_ppi: u32,
    #[inspect(skip)]
    vps: Vec<HvfVpInner>,
    gicd: gic::Distributor,
    guest_memory: GuestMemory,
    vmtime: VmTimeAccess,
    hv1: HvfHv1State,
    #[inspect(with = "|x| inspect::adhoc(|req| inspect::iter_by_index(&*x.lock()).inspect(req))")]
    mappings: Mutex<Vec<MemoryRange>>,
    synic_ports: virt::synic::SynicPortMap,
    #[inspect(skip)]
    ipa_range: IntermPhysAddrSize,
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
            if let Err(err) = unsafe { abi::hv_vcpus_exit(&vcpu, 1) }.chk() {
                tracelimit::error_ratelimited!(?err, "failed to force HVF vCPU exit");
            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IdRegisters {
    pfr0: u64,
    pfr1: u64,
    dfr0: u64,
    mmfr0: u64,
    mmfr2: u64,
}

impl IdRegisters {
    fn read(vcpu: &HvfVcpu) -> Result<Self, HvfError> {
        Ok(Self {
            pfr0: vcpu.sys_reg(abi::HvSysReg::ID_AA64PFR0_EL1)?,
            pfr1: vcpu.sys_reg(abi::HvSysReg::ID_AA64PFR1_EL1)?,
            dfr0: vcpu.sys_reg(abi::HvSysReg::ID_AA64DFR0_EL1)?,
            mmfr0: vcpu.sys_reg(abi::HvSysReg::ID_AA64MMFR0_EL1)?,
            mmfr2: vcpu.sys_reg(abi::HvSysReg::ID_AA64MMFR2_EL1)?,
        })
    }

    fn install(self, vcpu: &mut HvfVcpu) -> anyhow::Result<()> {
        for (register, expected, name) in [
            (abi::HvSysReg::ID_AA64PFR0_EL1, self.pfr0, "ID_AA64PFR0_EL1"),
            (abi::HvSysReg::ID_AA64PFR1_EL1, self.pfr1, "ID_AA64PFR1_EL1"),
            (abi::HvSysReg::ID_AA64DFR0_EL1, self.dfr0, "ID_AA64DFR0_EL1"),
            (
                abi::HvSysReg::ID_AA64MMFR0_EL1,
                self.mmfr0,
                "ID_AA64MMFR0_EL1",
            ),
            (
                abi::HvSysReg::ID_AA64MMFR2_EL1,
                self.mmfr2,
                "ID_AA64MMFR2_EL1",
            ),
        ] {
            vcpu.set_sys_reg(register, expected)
                .with_context(|| format!("failed to set {name}"))?;
            let actual = vcpu
                .sys_reg(register)
                .with_context(|| format!("failed to read back {name}"))?;
            anyhow::ensure!(
                actual == expected,
                "{name} readback mismatch: expected {expected:#x}, got {actual:#x}"
            );
        }
        Ok(())
    }
}

/// Derives the guest CPU model from HVF's host capability baseline.
///
/// Arm DDI 0601 (2026-06) defines `ID_AA64PFR0_EL1.GIC=0b0001` as the
/// GICv3/v4 system-register interface. The default HVF VM does not enable EL2,
/// and this backend does not preserve PMU, SVE, SME, NV, CnP, or EL3 state.
fn id_register_policy(host: IdRegisters, ipa_range: IntermPhysAddrSize) -> IdRegisters {
    let pfr0 = ProcessorFeatures0El1::from(host.pfr0)
        .with_gic(GicCpuInterface::GICV3_OR_GICV4)
        .with_el2(0)
        .with_el3(0)
        .with_sve(0);
    let pfr1 = ProcessorFeatures1El1::from(host.pfr1).with_sme(0);
    let dfr0 = DebugFeatures0El1::from(host.dfr0).with_pmu_ver(0);
    let mmfr0 = MmFeatures0El1::from(host.mmfr0).with_pa_range(ipa_range);
    let mmfr2 = MmFeatures2El1::from(host.mmfr2).with_cnp(0).with_nv(0);

    IdRegisters {
        pfr0: pfr0.into(),
        pfr1: pfr1.into(),
        dfr0: dfr0.into(),
        mmfr0: mmfr0.into(),
        mmfr2: mmfr2.into(),
    }
}

impl BindProcessor for HvfProcessorBinder {
    type Processor<'a> = HvfProcessor<'a>;
    type Error = Error;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        let mut vcpu = HvfVcpu::new()?;

        let state = self.state.take().unwrap();
        let inner = &self.partition.vps[self.vp_index.index() as usize];

        id_register_policy(IdRegisters::read(&vcpu)?, self.partition.ipa_range)
            .install(&mut vcpu)?;
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
        // SAFETY: this vCPU belongs to the current thread.
        if let Err(err) = unsafe { abi::hv_vcpu_destroy(self.vcpu) }.chk() {
            tracing::error!(?err, "failed to destroy HVF vCPU");
        }
    }
}

const MAX_HOST_TIMER_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// Converts an architected counter comparison into a bounded host wait.
///
/// Arm DDI 0601 (2026-06), `CNTVCT_EL0` and `CNTV_CVAL_EL0`, defines the
/// virtual timer against the monotonically increasing 64-bit system counter.
/// A guest can program a compare value beyond the range of the host's
/// `Instant`. Bound each host wait to the same one-day horizon used by the VM
/// time keeper, then recompute without changing the guest-visible deadline.
fn vtimer_wait_duration(counter: u64, compare: u64, frequency: NonZeroU64) -> Option<Duration> {
    if compare <= counter {
        return None;
    }

    let ticks = compare - counter;
    let frequency = frequency.get();
    Some(
        Duration::new(
            ticks / frequency,
            ((ticks % frequency) as u128 * 1_000_000_000 / frequency as u128) as u32,
        )
        .min(MAX_HOST_TIMER_WAIT),
    )
}

fn read_cntfrq() -> u64 {
    let freq: u64;
    // SAFETY: CNTFRQ_EL0 is unprivileged-readable and has no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, cntfrq_el0",
            out(reg) freq,
            options(nomem, nostack, preserves_flags),
        );
    }
    freq
}

/// Decodes `ESR_EL2.ISS.Rt` for a trapped system-register instruction.
///
/// Arm DDI 0601 (2026-06), `ESR_EL2` System instruction traps, assigns
/// `Rt=31` to XZR. A read targeting XZR discards the result, while a write
/// sourcing XZR supplies zero.
fn system_register_operand(rt: u8) -> Option<u8> {
    (rt != 31).then_some(rt)
}

/// Returns whether a trapped WF* instruction is WFI.
///
/// Arm DDI 0601 (2026-06), `ESR_EL2.ISS.TI`, encodes WFI, WFE, WFIT, and WFET
/// as `0b00` through `0b11`. Only WFI enters this backend's interrupt wait;
/// the others return until event and timed-wait semantics are implemented.
fn trapped_wfx_is_wfi(iss: u32) -> bool {
    iss & 0b11 == 0
}

impl HvfProcessor<'_> {
    /// Reflects the physical and virtual Arm system-counter bases.
    ///
    /// Apple's `hv_vcpu.h` defines the virtual count as
    /// `CNTVCT_EL0 = mach_absolute_time() - vtimer_offset`; the physical count
    /// uses the unshifted host counter.
    fn read_counter_sysreg(&self, reg: SystemReg) -> Result<Option<u64>, HvfError> {
        let value = match reg {
            SystemReg::CNTPCT_EL0 => {
                // SAFETY: no requirements.
                unsafe { abi::mach_absolute_time() }
            }
            SystemReg::CNTVCT_EL0 => {
                let mut offset = 0;
                // SAFETY: `offset` is a valid out parameter.
                unsafe { abi::hv_vcpu_get_vtimer_offset(self.vcpu.vcpu, &mut offset) }.chk()?;
                // SAFETY: no requirements.
                unsafe { abi::mach_absolute_time() }.wrapping_sub(offset)
            }
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    fn hypercall(&mut self, _dev: &impl CpuIo, smccc: bool) {
        let guest_memory = &self.partition.guest_memory;
        let handler = HvfHypercallHandler::new(self);
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

    /// Computes the host deadline for an enabled, unmasked virtual timer.
    ///
    /// Arm DDI 0601 (2026-06), `CNTV_CTL_EL0.{ENABLE,IMASK,ISTATUS}` and
    /// `CNTV_CVAL_EL0`, defines when the level output is asserted. HVF reports
    /// that output only while the vCPU runs, so a VP parked after WFI must first
    /// wait until the same architected compare value, then re-enter HVF.
    fn vtimer_deadline(&self) -> anyhow::Result<Option<VmTime>> {
        const ENABLE: u64 = 1 << 0;
        const IMASK: u64 = 1 << 1;
        const ISTATUS: u64 = 1 << 2;

        let ctl = self
            .vcpu
            .sys_reg(abi::HvSysReg::CNTV_CTL_EL0)
            .context("failed to read CNTV_CTL_EL0")?;
        if ctl & ENABLE == 0 || ctl & IMASK != 0 {
            return Ok(None);
        }
        let now = self.vmtime.now();
        if ctl & ISTATUS != 0 {
            return Ok(Some(now));
        }

        let cval = self
            .vcpu
            .sys_reg(abi::HvSysReg::CNTV_CVAL_EL0)
            .context("failed to read CNTV_CVAL_EL0")?;
        let mut offset = 0;
        // SAFETY: `offset` is a valid out parameter.
        unsafe { abi::hv_vcpu_get_vtimer_offset(self.vcpu.vcpu, &mut offset) }
            .chk()
            .context("failed to read the virtual timer offset")?;
        // SAFETY: no requirements.
        let guest_now = unsafe { abi::mach_absolute_time() }.wrapping_sub(offset);
        let frequency = NonZeroU64::new(read_cntfrq()).context("CNTFRQ_EL0 reported zero")?;

        Ok(Some(
            vtimer_wait_duration(guest_now, cval, frequency)
                .map_or(now, |duration| now.wrapping_add(duration)),
        ))
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

impl Drop for HvfProcessor<'_> {
    fn drop(&mut self) {
        let waker = self.inner.waker.write().take();
        self.inner.vcpu.store(!0, Ordering::SeqCst);
        drop(waker);
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
                        .map_err(|err| dev.fatal_error(err.into()))?;
                        self.wfi = false;
                    }

                    if self.wfi {
                        // A pending interrupt clears WFI above. Otherwise, arm
                        // the architected virtual-timer deadline and park on the
                        // existing VP waker. Timer expiry re-enters HVF; the
                        // subsequent VTIMER_ACTIVATED exit raises the level PPI.
                        let vtimer_deadline = self
                            .vtimer_deadline()
                            .map_err(|err| dev.fatal_error(err.into()))?;
                        if let Some(deadline) = vtimer_deadline {
                            self.vmtime.set_timeout_if_before(deadline);
                        }
                        if self.vmtime.poll_timeout(cx).is_ready() {
                            if vtimer_deadline
                                .is_some_and(|deadline| !deadline.is_after(self.vmtime.now()))
                            {
                                self.wfi = false;
                            }
                            continue;
                        }
                        return Poll::Pending;
                    }

                    break Poll::Ready(Result::<_, VpHaltReason>::Ok(()));
                }
            })
            .await?;

            if !self
                .gicr
                .is_pending_or_active(self.partition.virt_timer_ppi)
            {
                // SAFETY: no requirements.
                unsafe {
                    abi::hv_vcpu_set_vtimer_mask(self.vcpu.vcpu, false)
                        .chk()
                        .map_err(|err| dev.fatal_error(err.into()))?;
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
                                let value = if let Some(value) =
                                    self.partition.gicd.read_sysreg(&mut self.gicr, reg)
                                {
                                    value
                                } else if let Some(value) = self
                                    .read_counter_sysreg(reg)
                                    .map_err(|err| dev.fatal_error(err.into()))?
                                {
                                    value
                                } else {
                                    tracelimit::warn_ratelimited!(
                                        ?reg,
                                        "returning zero for unknown system register"
                                    );
                                    0
                                };
                                if let Some(rt) = system_register_operand(iss.rt()) {
                                    self.vcpu.set_gp(rt, value);
                                }
                            } else {
                                let value = system_register_operand(iss.rt())
                                    .map_or(0, |rt| self.vcpu.gp(rt));
                                if !self.partition.gicd.write_sysreg(
                                    &mut self.gicr,
                                    reg,
                                    value,
                                    |index| self.partition.vps[index].wake(),
                                ) {
                                    tracelimit::warn_ratelimited!(
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
                            if trapped_wfx_is_wfi(exception.syndrome.iss()) {
                                self.wfi = true;
                            }
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
                    self.gicr.raise(self.partition.virt_timer_ppi);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_preserves_unowned_fields_and_hides_unvirtualized_features() {
        let host = IdRegisters {
            pfr0: u64::MAX,
            pfr1: u64::MAX,
            dfr0: u64::MAX,
            mmfr0: u64::MAX,
            mmfr2: u64::MAX,
        };
        let guest = id_register_policy(host, IntermPhysAddrSize::IPA_40_BITS_1_TB);

        let expected_pfr0 = ProcessorFeatures0El1::from(host.pfr0)
            .with_gic(GicCpuInterface::GICV3_OR_GICV4)
            .with_el2(0)
            .with_el3(0)
            .with_sve(0);
        let expected_pfr1 = ProcessorFeatures1El1::from(host.pfr1).with_sme(0);
        let expected_dfr0 = DebugFeatures0El1::from(host.dfr0).with_pmu_ver(0);
        let expected_mmfr0 =
            MmFeatures0El1::from(host.mmfr0).with_pa_range(IntermPhysAddrSize::IPA_40_BITS_1_TB);
        let expected_mmfr2 = MmFeatures2El1::from(host.mmfr2).with_cnp(0).with_nv(0);

        assert_eq!(guest.pfr0, expected_pfr0.into());
        assert_eq!(guest.pfr1, expected_pfr1.into());
        assert_eq!(guest.dfr0, expected_dfr0.into());
        assert_eq!(guest.mmfr0, expected_mmfr0.into());
        assert_eq!(guest.mmfr2, expected_mmfr2.into());

        assert_eq!(expected_pfr0.gic(), GicCpuInterface::GICV3_OR_GICV4);
        assert_eq!(expected_pfr0.el2(), 0);
        assert_eq!(expected_pfr0.el3(), 0);
        assert_eq!(expected_pfr0.sve(), 0);
        assert_eq!(expected_pfr1.sme(), 0);
        assert_eq!(expected_dfr0.pmu_ver(), 0);
        assert_eq!(
            expected_mmfr0.pa_range(),
            IntermPhysAddrSize::IPA_40_BITS_1_TB
        );
        assert_eq!(expected_mmfr2.cnp(), 0);
        assert_eq!(expected_mmfr2.nv(), 0);
    }

    #[test]
    fn policy_advertises_gicv3_and_queried_ipa_size() {
        let guest = id_register_policy(
            IdRegisters {
                pfr0: 0,
                pfr1: 0,
                dfr0: 0,
                mmfr0: 0,
                mmfr2: 0,
            },
            IntermPhysAddrSize::IPA_48_BITS_256_TB,
        );

        assert_eq!(
            ProcessorFeatures0El1::from(guest.pfr0).gic(),
            GicCpuInterface::GICV3_OR_GICV4
        );
        assert_eq!(
            MmFeatures0El1::from(guest.mmfr0).pa_range(),
            IntermPhysAddrSize::IPA_48_BITS_256_TB
        );
    }

    #[test]
    fn ipa_bit_length_maps_to_arm_parange() {
        for (bits, expected) in [
            (32, IntermPhysAddrSize::IPA_32_BITS_4_GB),
            (36, IntermPhysAddrSize::IPA_36_BITS_64_GB),
            (40, IntermPhysAddrSize::IPA_40_BITS_1_TB),
            (42, IntermPhysAddrSize::IPA_42_BITS_4_TB),
            (44, IntermPhysAddrSize::IPA_44_BITS_16_TB),
            (48, IntermPhysAddrSize::IPA_48_BITS_256_TB),
            (52, IntermPhysAddrSize::IPA_52_BITS_4_PB),
            (56, IntermPhysAddrSize::IPA_56_BITS_64_PB),
        ] {
            assert_eq!(
                IntermPhysAddrSize::from_ipa_bit_length(bits),
                Some(expected)
            );
        }
        assert_eq!(IntermPhysAddrSize::from_ipa_bit_length(39), None);
    }

    #[test]
    fn counter_comparison_distinguishes_expired_and_future_deadlines() {
        let frequency = NonZeroU64::new(10).unwrap();

        assert_eq!(vtimer_wait_duration(10, 10, frequency), None);
        assert_eq!(vtimer_wait_duration(11, 10, frequency), None);
        assert_eq!(
            vtimer_wait_duration(10, 25, frequency),
            Some(Duration::new(1, 500_000_000))
        );
    }

    #[test]
    fn counter_comparison_uses_architected_unsigned_ordering() {
        let frequency = NonZeroU64::new(1).unwrap();

        assert_eq!(
            vtimer_wait_duration(1, u64::MAX, frequency),
            Some(MAX_HOST_TIMER_WAIT)
        );
        assert_eq!(vtimer_wait_duration(u64::MAX - 10, 9, frequency), None);
    }

    #[test]
    fn system_register_rt_31_decodes_as_xzr() {
        assert_eq!(system_register_operand(0), Some(0));
        assert_eq!(system_register_operand(30), Some(30));
        assert_eq!(system_register_operand(31), None);
    }

    #[test]
    fn esr_ti_distinguishes_wfi_from_other_wait_instructions() {
        assert!(trapped_wfx_is_wfi(0b00));
        assert!(!trapped_wfx_is_wfi(0b01));
        assert!(!trapped_wfx_is_wfi(0b10));
        assert!(!trapped_wfx_is_wfi(0b11));
    }
}
