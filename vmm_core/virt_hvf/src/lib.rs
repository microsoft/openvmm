// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(all(target_os = "macos", guest_is_native, guest_arch = "aarch64"))]

//! A hypervisor backend using macos's Hypervisor framework.

// UNSAFETY: Calling Hypervisor framework APIs and manually managing memory.
#![expect(unsafe_code)]

mod abi;
mod hypercall;
mod vp_actor;
mod vp_state;

use crate::hypercall::HvfHypercallHandler;
use aarch64defs::Cpsr64;
use aarch64defs::ExceptionClass;
use aarch64defs::IssDataAbort;
use aarch64defs::IssSystem;
use aarch64defs::MpidrEl1;
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
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Poll;
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

const VM_IPA_BITS: u8 = 36;

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
        }
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

        // SAFETY: no safety requirements. A null configuration selects Apple's
        // default VM. Expose only the conservative 36-bit IPA subset so the
        // guest never constructs an address outside that default VM.
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
                .map(|vp_info| {
                    let power_state = if vp_info.base.vp_index.is_bsp() {
                        VP_ON
                    } else {
                        VP_OFF
                    };
                    HvfVpInner {
                        needs_yield: NeedsYield::new(),
                        message_queues: hv1_emulator::message_queues::MessageQueues::new(),
                        actor: vp_actor::VpActor::new(),
                        vp_info,
                        cpu_on: Default::default(),
                        power_state: AtomicU8::new(power_state),
                    }
                })
                .collect(),
            gicd,
            guest_memory: config.guest_memory.clone(),
            vmtime: self.config.vmtime.access("hvf"),
            hv1,
            mappings: Default::default(),
            synic_ports: Default::default(),
            id_registers: Default::default(),
            partition_info_page: AtomicU64::new(0),
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
                }),
            });
        }

        let synic_ports = Arc::new(virt::synic::SynicPorts::new(inner.clone()));

        let partition = HvfPartition { inner, synic_ports };
        Ok((partition, vps))
    }

    fn max_physical_address_size(&self) -> u8 {
        VM_IPA_BITS
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
                vp.notify();
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
                vp.notify();
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
                            let newly_pending = partition.gicd.raise_ppi(vp, vector);
                            if newly_pending {
                                partition.vps[vp.index() as usize].notify();
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
    id_registers: Mutex<Option<IdRegisters>>,
    partition_info_page: AtomicU64,
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
    message_queues: hv1_emulator::message_queues::MessageQueues,
    #[inspect(skip)]
    actor: vp_actor::VpActor,
    cpu_on: Mutex<Option<CpuOnState>>,
    #[inspect(skip)]
    power_state: AtomicU8,
}

const VP_OFF: u8 = 0;
const VP_ON_PENDING: u8 = 1;
const VP_ON: u8 = 2;
const PSCI_AFFINITY_ON: i32 = 0;
const PSCI_AFFINITY_OFF: i32 = 1;
const PSCI_AFFINITY_ON_PENDING: i32 = 2;

#[derive(Debug, Inspect)]
struct CpuOnState {
    pc: u64,
    x0: u64,
}

impl HvfVpInner {
    fn cancel_run(&self) {
        self.actor.cancel_run();
    }

    /// Requests this vCPU to observe work that has already been published.
    fn notify(&self) {
        self.actor.notify();
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
}

// Arm A-profile Architecture Registers, DDI 0601 (2026-06):
// ID_AA64PFR0_EL1.{GIC,EL2,EL3,SVE}, ID_AA64PFR1_EL1.SME,
// ID_AA64DFR0_EL1.PMUVer, ID_AA64MMFR0_EL1.PARange, and
// ID_AA64MMFR2_EL1.{CnP,NV}. These masks define one VM-wide virtual CPU model.
/// GICv3/v4 system-register CPU interface.
const ID_AA64PFR0_EL1_GIC_CPUIF: u64 = 1 << 24;
const ID_AA64PFR0_EL1_GIC: u64 = 0xf << 24;
const ID_AA64MMFR0_EL1_PARANGE: u64 = 0xf;
const ID_AA64MMFR0_EL1_PARANGE_36BIT: u64 = 0b0001;
const ID_AA64DFR0_EL1_PMUVER: u64 = 0xf << 8;
const ID_AA64MMFR2_EL1_CNP: u64 = 0xf;
const ID_AA64MMFR2_EL1_NV: u64 = 0xf << 24;
const ID_AA64PFR0_EL1_EL2: u64 = 0xf << 8;
const ID_AA64PFR0_EL1_EL3: u64 = 0xf << 12;
const ID_AA64PFR0_EL1_SVE: u64 = 0xf << 32;
const ID_AA64PFR1_EL1_SME: u64 = 0xf << 24;

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

/// Derives the VM-wide virtual CPU model from HVF's host capability baseline.
///
/// Arm DDI 0601 (2026-06) defines the fields above. OpenVMM advertises the
/// software GIC system-register interface, hides EL2 because the default HVF
/// configuration does not enable it, exposes the conservative 36-bit IPA
/// subset of Apple's default VM, and hides host features whose state it
/// cannot safely virtualize. PMU discovery remains hidden; [`PmuState`] is a
/// compatibility shim for guests that access the cycle counter directly.
fn id_register_policy(host: IdRegisters) -> IdRegisters {
    IdRegisters {
        pfr0: (host.pfr0
            & !(ID_AA64PFR0_EL1_GIC
                | ID_AA64PFR0_EL1_EL2
                | ID_AA64PFR0_EL1_EL3
                | ID_AA64PFR0_EL1_SVE))
            | ID_AA64PFR0_EL1_GIC_CPUIF,
        pfr1: host.pfr1 & !ID_AA64PFR1_EL1_SME,
        dfr0: host.dfr0 & !ID_AA64DFR0_EL1_PMUVER,
        mmfr0: (host.mmfr0 & !ID_AA64MMFR0_EL1_PARANGE) | ID_AA64MMFR0_EL1_PARANGE_36BIT,
        mmfr2: host.mmfr2 & !(ID_AA64MMFR2_EL1_CNP | ID_AA64MMFR2_EL1_NV),
    }
}

#[cfg(test)]
mod id_register_tests {
    use super::*;

    #[test]
    fn policy_changes_only_virtualized_id_fields() {
        let host = IdRegisters {
            pfr0: u64::MAX,
            pfr1: u64::MAX,
            dfr0: u64::MAX,
            mmfr0: u64::MAX,
            mmfr2: u64::MAX,
        };
        let guest = id_register_policy(host);
        let pfr0_mask =
            ID_AA64PFR0_EL1_GIC | ID_AA64PFR0_EL1_EL2 | ID_AA64PFR0_EL1_EL3 | ID_AA64PFR0_EL1_SVE;

        assert_eq!(guest.pfr0 & !pfr0_mask, host.pfr0 & !pfr0_mask);
        assert_eq!(guest.pfr0 & pfr0_mask, ID_AA64PFR0_EL1_GIC_CPUIF);
        assert_eq!(guest.pfr1, host.pfr1 & !ID_AA64PFR1_EL1_SME);
        assert_eq!(guest.dfr0, host.dfr0 & !ID_AA64DFR0_EL1_PMUVER);
        assert_eq!(
            guest.mmfr0,
            (host.mmfr0 & !ID_AA64MMFR0_EL1_PARANGE) | ID_AA64MMFR0_EL1_PARANGE_36BIT
        );
        assert_eq!(
            guest.mmfr2,
            host.mmfr2 & !(ID_AA64MMFR2_EL1_CNP | ID_AA64MMFR2_EL1_NV)
        );
    }

    #[test]
    fn policy_adds_required_virtual_features() {
        let guest = id_register_policy(IdRegisters {
            pfr0: 0,
            pfr1: 0,
            dfr0: 0,
            mmfr0: 0,
            mmfr2: 0,
        });

        assert_eq!(guest.pfr0, ID_AA64PFR0_EL1_GIC_CPUIF);
        assert_eq!(guest.mmfr0, ID_AA64MMFR0_EL1_PARANGE_36BIT);
    }
}

/// Arm DDI 0601 (2026-06), `PMCR_EL0.E` (bit 0): cycle counter enable.
const PMCR_EL0_E: u64 = 1 << 0;
/// Arm DDI 0601 (2026-06), `PMCR_EL0.C` (bit 2): cycle counter reset.
const PMCR_EL0_C: u64 = 1 << 2;
/// Arm DDI 0601 (2026-06), `PMCR_EL0.LC` (bit 6): 64-bit cycle counter.
const PMCR_EL0_LC: u64 = 1 << 6;
/// Arm DDI 0601 (2026-06), `PMCNTENSET_EL0.C` (bit 31).
const PMCNTEN_C: u32 = 1 << 31;

impl BindProcessor for HvfProcessorBinder {
    type Processor<'a> = HvfProcessor<'a>;
    type Error = Error;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        let mut vcpu = HvfVcpu::new()?;

        let state = self.state.take().unwrap();
        let inner = &self.partition.vps[self.vp_index.index() as usize];

        let id_registers = {
            let mut shared = self.partition.id_registers.lock();
            match *shared {
                Some(id_registers) => id_registers,
                None => {
                    let id_registers = id_register_policy(IdRegisters::read(&vcpu)?);
                    *shared = Some(id_registers);
                    id_registers
                }
            }
        };
        id_registers.install(&mut vcpu)?;
        // Set the MPIDR.
        vcpu.set_sys_reg(abi::HvSysReg::MPIDR_EL1, inner.vp_info.mpidr.into())?;

        // Record the live HVF vcpu id in the wake actor (enables the
        // running-state `hv_vcpus_exit` wake).
        inner.actor.set_vcpu(vcpu.vcpu);

        let mut vp = HvfProcessor {
            partition: &self.partition,
            inner,
            vcpu,
            wfi: false,
            on: inner.vp_info.base.vp_index.is_bsp(),
            gicr: state.gicr,
            hv1: state.hv1,
            vmtime: state.vmtime,
            pmu: PmuState::default(),
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

/// Compatibility model for guests that access the PMU cycle counter directly.
///
/// Arm DDI 0487M.c, Performance Monitors Extension, requires both
/// `PMCR_EL0.E` and `PMCNTENSET_EL0.C` before `PMCCNTR_EL0` counts; writing
/// `PMCR_EL0.C` resets it. The fixed rate is Hyper-V compatibility policy, not
/// an architectural CPU-frequency claim.
#[derive(Debug, Default, Inspect)]
struct PmuState {
    pmcr_enabled: bool,
    /// Logical PMCCNTR_EL0 value captured at the last re-base point.
    cycle_offset: u64,
    /// VM time (100ns units) captured at the last re-base point.
    cycle_base_100ns: u64,
    /// PMCNTENSET_EL0/PMCNTENCLR_EL0 (cycle-counter bit 31 + event bits).
    counter_enable: u32,
    /// PMINTENSET_EL1/PMINTENCLR_EL1.
    int_enable: u32,
    /// PMUSERENR_EL0 (EL0 access controls).
    userenr: u32,
    /// PMCCFILTR_EL0.
    ccfiltr: u32,
    /// PMSELR_EL0 counter selector.
    selr: u32,
}

impl PmuState {
    /// Guests calibrate this steady synthetic rate against the architected timer.
    const CYCLES_PER_100NS: u64 = 300;

    fn counting(&self) -> bool {
        self.pmcr_enabled && self.counter_enable & PMCNTEN_C != 0
    }

    fn pmccntr(&self, now_100ns: u64) -> u64 {
        if self.counting() {
            let elapsed = now_100ns.wrapping_sub(self.cycle_base_100ns);
            self.cycle_offset
                .wrapping_add(elapsed.wrapping_mul(Self::CYCLES_PER_100NS))
        } else {
            self.cycle_offset
        }
    }

    fn rebase(&mut self, value: u64, now_100ns: u64) {
        self.cycle_offset = value;
        self.cycle_base_100ns = now_100ns;
    }

    fn read_sysreg(&self, reg: SystemReg, now_100ns: u64) -> Option<u64> {
        let value = match reg {
            SystemReg::PMCCNTR_EL0 => self.pmccntr(now_100ns),
            SystemReg::PMCR_EL0 => PMCR_EL0_LC | if self.pmcr_enabled { PMCR_EL0_E } else { 0 },
            SystemReg::PMCNTENSET_EL0 | SystemReg::PMCNTENCLR_EL0 => self.counter_enable.into(),
            SystemReg::PMINTENSET_EL1 | SystemReg::PMINTENCLR_EL1 => self.int_enable.into(),
            SystemReg::PMUSERENR_EL0 => self.userenr.into(),
            SystemReg::PMCCFILTR_EL0 => self.ccfiltr.into(),
            SystemReg::PMSELR_EL0 => self.selr.into(),
            SystemReg::PMOVSSET_EL0 | SystemReg::PMOVSCLR_EL0 => 0,
            SystemReg::PMCEID0_EL0 | SystemReg::PMCEID1_EL0 => 0,
            _ => return None,
        };
        Some(value)
    }

    fn write_sysreg(&mut self, reg: SystemReg, value: u64, now_100ns: u64) -> bool {
        match reg {
            SystemReg::PMCR_EL0 => {
                let cur = self.pmccntr(now_100ns);
                self.pmcr_enabled = value & PMCR_EL0_E != 0;
                self.rebase(cur, now_100ns);
                if value & PMCR_EL0_C != 0 {
                    self.rebase(0, now_100ns);
                }
            }
            SystemReg::PMCCNTR_EL0 => self.rebase(value, now_100ns),
            SystemReg::PMCNTENSET_EL0 | SystemReg::PMCNTENCLR_EL0 => {
                let cur = self.pmccntr(now_100ns);
                if reg == SystemReg::PMCNTENSET_EL0 {
                    self.counter_enable |= value as u32;
                } else {
                    self.counter_enable &= !(value as u32);
                }
                self.rebase(cur, now_100ns);
            }
            SystemReg::PMINTENSET_EL1 => self.int_enable |= value as u32,
            SystemReg::PMINTENCLR_EL1 => self.int_enable &= !(value as u32),
            SystemReg::PMUSERENR_EL0 => self.userenr = value as u32,
            SystemReg::PMCCFILTR_EL0 => self.ccfiltr = value as u32,
            SystemReg::PMSELR_EL0 => self.selr = value as u32,
            SystemReg::PMOVSSET_EL0 | SystemReg::PMOVSCLR_EL0 => {}
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod pmu_tests {
    use super::*;

    #[test]
    fn cycle_counter_requires_both_enable_bits() {
        let mut pmu = PmuState::default();

        assert!(pmu.write_sysreg(SystemReg::PMCR_EL0, PMCR_EL0_E, 10));
        assert_eq!(pmu.read_sysreg(SystemReg::PMCCNTR_EL0, 12), Some(0));

        assert!(pmu.write_sysreg(SystemReg::PMCNTENSET_EL0, PMCNTEN_C.into(), 12));
        assert_eq!(
            pmu.read_sysreg(SystemReg::PMCCNTR_EL0, 14),
            Some(2 * PmuState::CYCLES_PER_100NS)
        );

        assert!(pmu.write_sysreg(SystemReg::PMCNTENCLR_EL0, PMCNTEN_C.into(), 14));
        assert_eq!(
            pmu.read_sysreg(SystemReg::PMCCNTR_EL0, 20),
            Some(2 * PmuState::CYCLES_PER_100NS)
        );
    }

    #[test]
    fn cycle_counter_reset_rebases_while_running() {
        let mut pmu = PmuState::default();

        assert!(pmu.write_sysreg(SystemReg::PMCNTENSET_EL0, PMCNTEN_C.into(), 10));
        assert!(pmu.write_sysreg(SystemReg::PMCR_EL0, PMCR_EL0_E, 10));
        assert!(pmu.write_sysreg(SystemReg::PMCR_EL0, PMCR_EL0_E | PMCR_EL0_C, 12));
        assert_eq!(
            pmu.read_sysreg(SystemReg::PMCCNTR_EL0, 13),
            Some(PmuState::CYCLES_PER_100NS)
        );
    }

    #[test]
    fn pmu_reports_no_event_counters() {
        let pmu = PmuState::default();

        assert_eq!(pmu.read_sysreg(SystemReg::PMCEID0_EL0, 0), Some(0));
        assert_eq!(pmu.read_sysreg(SystemReg::PMCEID1_EL0, 0), Some(0));
        assert_eq!(pmu.read_sysreg(SystemReg::PMCR_EL0, 0), Some(PMCR_EL0_LC));
    }
}

/// Arm DDI 0487M.c A64 register encodings: register 31 denotes XZR in the
/// trapped load/store and system-register forms handled by this backend.
fn reg_is_xzr(reg: u8) -> bool {
    reg == 31
}

/// Arm DDI 0487M.c `ESR_ELx.ISS.TI`: zero identifies WFI. WFE/WFIT/WFET
/// return immediately until this backend models their event/timeout semantics.
fn trapped_wfx_is_wfi(iss: u32) -> bool {
    iss & 0b11 == 0
}

#[cfg(test)]
mod xzr_tests {
    use super::reg_is_xzr;

    #[test]
    fn only_reg_31_is_xzr() {
        for reg in 0..=30u8 {
            assert!(!reg_is_xzr(reg), "reg {reg} must be a GP register, not XZR");
        }
        assert!(reg_is_xzr(31), "reg 31 must decode as XZR");
    }
}

#[cfg(test)]
mod wfx_tests {
    use super::trapped_wfx_is_wfi;

    #[test]
    fn only_wfi_enters_interrupt_park() {
        assert!(trapped_wfx_is_wfi(0b00));
        assert!(!trapped_wfx_is_wfi(0b01));
        assert!(!trapped_wfx_is_wfi(0b10));
        assert!(!trapped_wfx_is_wfi(0b11));
    }
}

/// Reads the counter frequency (`CNTFRQ_EL0`) in Hz — the tick rate shared by
/// `CNTVCT_EL0` and the guest's virtual timer.
fn read_cntfrq() -> u64 {
    let freq: u64;
    // SAFETY: CNTFRQ_EL0 is unprivileged-readable on AArch64 with no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, cntfrq_el0",
            out(reg) freq,
            options(nomem, nostack, preserves_flags),
        );
    }
    freq
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
    pmu: PmuState,
}

#[derive(Debug, Inspect)]
struct HvfVcpu {
    vcpu: u64,
    #[inspect(skip)]
    exit: ExitPtr,
    #[inspect(skip)]
    valid: bool,
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
            valid: true,
        })
    }

    fn destroy(&mut self) -> Result<(), HvfError> {
        if self.valid {
            // SAFETY: this vCPU belongs to the current thread.
            unsafe { abi::hv_vcpu_destroy(self.vcpu) }.chk()?;
            self.valid = false;
        }
        Ok(())
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
        if let Err(err) = self.destroy() {
            tracing::error!(?err, "failed to destroy HVF vCPU");
        }
    }
}

const MAX_VTIMER_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// Converts a generic-timer compare value to a bounded host wait. `None` means
/// the unsigned counter has already reached the compare value.
///
/// Arm DDI 0487M.c, Generic Timer, and DDI 0601 (2026-06)
/// `CNTVCT_EL0`/`CNTV_CVAL_EL0` define the unsigned counter comparison. The
/// one-day bound is host scheduling policy; the architectural deadline is
/// recomputed after each bounded wait.
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
        .min(MAX_VTIMER_WAIT),
    )
}

#[cfg(test)]
mod vtimer_tests {
    use super::*;

    #[test]
    fn deadline_conversion_handles_expired_and_future_values() {
        let frequency = NonZeroU64::new(10).unwrap();

        assert_eq!(vtimer_wait_duration(10, 10, frequency), None);
        assert_eq!(vtimer_wait_duration(11, 10, frequency), None);
        assert_eq!(
            vtimer_wait_duration(10, 25, frequency),
            Some(Duration::new(1, 500_000_000))
        );
    }

    #[test]
    fn deadline_conversion_uses_unsigned_counter_ordering() {
        let frequency = NonZeroU64::new(1).unwrap();

        assert_eq!(
            vtimer_wait_duration(1, u64::MAX, frequency),
            Some(MAX_VTIMER_WAIT)
        );
        assert_eq!(vtimer_wait_duration(u64::MAX - 10, 9, frequency), None);
    }
}

impl HvfProcessor<'_> {
    /// Reflects Apple's physical/virtual counter basis for trapped reads.
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

    /// Computes the virtual-timer deadline while the vCPU is parked outside HVF.
    ///
    /// Arm DDI 0601 (2026-06) defines `CNTV_CTL_EL0` ENABLE/IMASK/ISTATUS,
    /// `CNTV_CVAL_EL0`, and `CNTFRQ_EL0`. Apple's `hv_vcpu.h` defines
    /// `CNTVCT_EL0 = mach_absolute_time() - vtimer_offset`.
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

        let mut offset = 0u64;
        // SAFETY: `offset` is a valid out-param.
        unsafe { abi::hv_vcpu_get_vtimer_offset(self.vcpu.vcpu, &mut offset) }
            .chk()
            .context("failed to read the virtual timer offset")?;
        // SAFETY: no requirements.
        let guest_now = unsafe { abi::mach_absolute_time() }.wrapping_sub(offset);

        let freq = NonZeroU64::new(read_cntfrq()).context("CNTFRQ_EL0 reported zero")?;

        Ok(Some(
            vtimer_wait_duration(guest_now, cval, freq)
                .map_or(now, |duration| now.wrapping_add(duration)),
        ))
    }

    fn recreate_vcpu(&mut self) -> Result<(), Error> {
        let id_registers = self
            .partition
            .id_registers
            .lock()
            .as_ref()
            .copied()
            .context("missing partition ID-register model")?;
        let mpidr = self.inner.vp_info.mpidr;
        let current_vcpu = &mut self.vcpu;
        let vcpu = self.inner.actor.replace_vcpu(|| -> Result<_, Error> {
            current_vcpu
                .destroy()
                .context("failed to destroy vCPU before recreation")?;

            let mut vcpu = HvfVcpu::new()?;
            id_registers.install(&mut vcpu)?;
            vcpu.set_sys_reg(abi::HvSysReg::MPIDR_EL1, mpidr.into())?;
            Ok((vcpu.vcpu, vcpu))
        })?;
        self.vcpu = vcpu;
        Ok(())
    }

    fn set_reset_registers(&mut self, cpu_on: Option<CpuOnState>) -> Result<(), Error> {
        let mut registers =
            virt::aarch64::vp::Registers::at_reset(&self.partition.caps, &self.inner.vp_info);
        if let Some(cpu_on) = cpu_on {
            registers.pc = cpu_on.pc;
            registers.x0 = cpu_on.x0;
        }
        let system_registers =
            virt::aarch64::vp::SystemRegisters::at_reset(&self.partition.caps, &self.inner.vp_info);
        let mut state = self.access_state(Vtl::Vtl0);
        state.set_registers(&registers)?;
        state.set_system_registers(&system_registers)?;
        Ok(())
    }

    fn power_on(&mut self, cpu_on: CpuOnState) -> Result<(), Error> {
        self.recreate_vcpu()?;
        self.set_reset_registers(Some(cpu_on))?;
        self.pmu = PmuState::default();
        self.wfi = false;
        self.on = true;
        self.inner.power_state.store(VP_ON, Ordering::Release);
        Ok(())
    }

    /// Handles the Arm SMC Calling Convention subset exposed to the guest.
    ///
    /// Arm DEN0028G EAC1 (SMCCC v1.6) defines the function-ID owner, width,
    /// fast-call, argument, and result conventions. This backend advertises
    /// SMCCC v1.1, the first revision containing VERSION and ARCH_FEATURES.
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

    /// Handles the implemented PSCI v1.0 power-state subset.
    ///
    /// Arm DEN0022F.b defines CPU_ON/OFF, AFFINITY_INFO, SYSTEM_OFF/RESET,
    /// the ON/OFF/ON_PENDING states, and CPU_ON entry PC plus X0 context.
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
                    match vp.power_state.compare_exchange(
                        VP_OFF,
                        VP_ON_PENDING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            *cpu_on = Some(CpuOnState {
                                pc: entry_point,
                                x0: context_id,
                            });
                            drop(cpu_on);
                            vp.notify();
                            PsciError::SUCCESS.0
                        }
                        Err(VP_ON_PENDING) => PsciError::ON_PENDING.0,
                        Err(VP_ON) => PsciError::ALREADY_ON.0,
                        Err(_) => PsciError::INTERNAL_FAILURE.0,
                    }
                } else {
                    PsciError::INVALID_PARAMETERS.0
                }
            }
            SmcCall::CPU_OFF => {
                self.on = false;
                self.inner.power_state.store(VP_OFF, Ordering::Release);
                PsciError::SUCCESS.0
            }
            SmcCall::AFFINITY_INFO => {
                let target_cpu = self.vcpu.gp(1) & mask;
                let lowest_affinity_level = self.vcpu.gp(2) & mask;
                if lowest_affinity_level != 0 {
                    PsciError::INVALID_PARAMETERS.0
                } else if let Some(vp) = self.partition.vps.iter().find(|vp| {
                    u64::from(vp.vp_info.mpidr) & u64::from(MpidrEl1::AFFINITY_MASK) == target_cpu
                }) {
                    match vp.power_state.load(Ordering::Acquire) {
                        VP_OFF => PSCI_AFFINITY_OFF,
                        VP_ON_PENDING => PSCI_AFFINITY_ON_PENDING,
                        VP_ON => PSCI_AFFINITY_ON,
                        _ => PsciError::INTERNAL_FAILURE.0,
                    }
                } else {
                    PsciError::INVALID_PARAMETERS.0
                }
            }
            SmcCall::SYSTEM_RESET => {
                return Err(VpHaltReason::Reset);
            }
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
        if let Err(err) = self.inner.actor.remove_vcpu(|| self.vcpu.destroy()) {
            tracing::error!(?err, "failed to remove HVF vCPU");
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

        loop {
            self.inner.needs_yield.maybe_yield().await;

            poll_fn(|cx| {
                loop {
                    stop.check()?;

                    // Capture notifications before scanning persistent work.
                    let scan = self.inner.actor.begin_scan();

                    if let Some(cpu_on) = self.inner.cpu_on.lock().take() {
                        if self.on {
                            return Poll::Ready(Err(dev.fatal_error(
                                anyhow::anyhow!("received PSCI CPU_ON for an online vCPU").into(),
                            )));
                        }
                        self.power_on(cpu_on)
                            .map_err(|err| dev.fatal_error(err.into()))?;
                    }

                    if !self.on {
                        // Secondary vCPU not yet powered on: park until a
                        // PSCI CPU_ON publishes a start request and notifies us.
                        match self
                            .inner
                            .actor
                            .try_park(scan, cx.waker(), || self.inner.cpu_on.lock().is_none())
                        {
                            vp_actor::ParkDecision::Parked => return Poll::Pending,
                            vp_actor::ParkDecision::Rescan => continue,
                        }
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
                        // Arm DDI 0487M.c defines a pending interrupt or expired
                        // virtual timer as a WFI wake event. While this task is
                        // parked outside `hv_vcpu_run`, register the architected
                        // deadline with vmtime and the interrupt path with VpActor.
                        if let Some(deadline) = self
                            .vtimer_deadline()
                            .map_err(|err| dev.fatal_error(err.into()))?
                        {
                            self.vmtime.set_timeout_if_before(deadline);
                        }
                        if self.vmtime.poll_timeout(cx).is_ready() {
                            self.wfi = false;
                            continue;
                        }
                        match self.inner.actor.try_park(scan, cx.waker(), || {
                            !self.partition.gicd.irq_pending(&self.gicr)
                        }) {
                            vp_actor::ParkDecision::Parked => {
                                return Poll::Pending;
                            }
                            vp_actor::ParkDecision::Rescan => continue,
                        }
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
                unsafe { abi::hv_vcpu_set_vtimer_mask(self.vcpu.vcpu, false).chk() }
                    .map_err(|err| dev.fatal_error(err.into()))?;
            }

            // A yield requested while CPU_ON/reset replaces the vCPU cannot
            // cancel the temporarily unpublished identifier. Recheck the
            // persistent yield latch after replacement; requests after this
            // point use Apple's sticky `hv_vcpus_exit` on the new identifier.
            self.inner.needs_yield.maybe_yield().await;

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

                            // SRT 31 encodes XZR, not SP.
                            let reg = iss.srt();

                            if iss.wnr() {
                                let data = if reg_is_xzr(reg) {
                                    0
                                } else {
                                    self.vcpu.gp(reg)
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
                            } else if !reg_is_xzr(reg) {
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
                            let now = self.vmtime.now().as_100ns();
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
                                } else if let Some(value) = self.pmu.read_sysreg(reg, now) {
                                    value
                                } else if reg == SystemReg::OSLSR_EL1 {
                                    // ARMv8 mandates the OS Lock; its reset value is
                                    // OSLM=0b10 (bits[3,0]) ⇒ 0x8, OSLK=0 (unlocked).
                                    // Report the lock as implemented and unlocked.
                                    0x8
                                } else {
                                    tracelimit::warn_ratelimited!(
                                        ?reg,
                                        pc = self.vcpu.pc(),
                                        "returning zero for unknown system register"
                                    );
                                    0
                                };
                                // Reads targeting XZR still perform register side effects.
                                if !reg_is_xzr(iss.rt()) {
                                    self.vcpu.set_gp(iss.rt(), value);
                                }
                            } else {
                                let value = if reg_is_xzr(iss.rt()) {
                                    0
                                } else {
                                    self.vcpu.gp(iss.rt())
                                };
                                let handled_by_gic = self.partition.gicd.write_sysreg(
                                    &mut self.gicr,
                                    reg,
                                    value,
                                    |index| self.partition.vps[index].notify(),
                                );
                                if !handled_by_gic && !self.pmu.write_sysreg(reg, value, now) {
                                    tracelimit::warn_ratelimited!(
                                        ?reg,
                                        value,
                                        pc = self.vcpu.pc(),
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
