// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements support for KVM on aarch64.
//! It is unsatisfactory (e.g. DeviceTree generation is disjoint from this code, and
//! this code does not rely on the KVM_CAP to see what is actually supported), but it
//! is a start providing assurance that OpenVMM virtualization model is appropriate for
//! KVM/aarch64.

#![expect(dead_code)]
#![cfg(all(target_os = "linux", guest_arch = "aarch64"))]

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use crate::KvmRunVpError;
use aarch64defs::SystemReg;
use bitfield_struct::bitfield;
use core::panic;
use hvdef::Vtl;
use inspect::Inspect;
use inspect::InspectMut;
use kvm::KVM_CAP_ARM_VM_IPA_SIZE;
use kvm::KVM_DEV_ARM_VGIC_CTRL_INIT;
use kvm::KVM_DEV_ARM_VGIC_GRP_ADDR;
use kvm::KVM_DEV_ARM_VGIC_GRP_CTRL;
use kvm::KVM_DEV_ARM_VGIC_GRP_NR_IRQS;
use kvm::KVM_VGIC_V3_ADDR_TYPE_DIST;
use kvm::KVM_VGIC_V3_ADDR_TYPE_REDIST;
use kvm::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3;
use kvm::kvm_regs;
use kvm::user_pt_regs;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use virt::NeedsYield;
use virt::PartitionCapabilities;
use virt::ProtoPartitionConfig;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::vp::Registers;
use virt::vp::SystemRegisters;
use virt::x86::DebugState;
use vm_topology::processor::aarch64::Aarch64VpInfo;
use vmcore::reference_time::ReferenceTimeSource;
use vmcore::vmtime::VmTimeAccess;

// linux/arch/arm64/include/asm/sysreg.h

const REG_ARM_COPROC_SHIFT: u64 = 16;
const REG_ARM_CORE: u64 = 0x0010 << REG_ARM_COPROC_SHIFT;
const REG_ARM64_SYSREG: u64 = 0x0013 << REG_ARM_COPROC_SHIFT;

const REG_ARM64: u64 = 0x6000000000000000;
const REG_ARM64_CORE_BASE: u64 = REG_ARM64 | REG_ARM_CORE;
const REG_ARM64_SYSREG_BASE: u64 = REG_ARM64 | REG_ARM64_SYSREG;

const REG_SIZE_U8: u64 = 0x0000000000000000;
const REG_SIZE_U16: u64 = 0x0010000000000000;
const REG_SIZE_U32: u64 = 0x0020000000000000;
const REG_SIZE_U64: u64 = 0x0030000000000000;
const REG_SIZE_U128: u64 = 0x0040000000000000;
const REG_SIZE_U256: u64 = 0x0050000000000000;
const REG_SIZE_U512: u64 = 0x0060000000000000;
const REG_SIZE_U1024: u64 = 0x0070000000000000;
const REG_SIZE_U2048: u64 = 0x0080000000000000;

const fn reg64(offset: u64) -> u64 {
    let offset = offset / (size_of::<u32>() as u64);
    offset | REG_ARM64_CORE_BASE | REG_SIZE_U64
}

const fn user_x_reg64(x: u64) -> u64 {
    let byte_offset = x * (size_of::<u64>() as u64);
    let reg_start = std::mem::offset_of!(user_pt_regs, regs) as u64 + byte_offset;
    reg64(reg_start)
}

const fn user_pstate_reg64() -> u64 {
    let reg_start = std::mem::offset_of!(user_pt_regs, pstate) as u64;
    reg64(reg_start)
}

const fn user_sp_reg64() -> u64 {
    let reg_start = std::mem::offset_of!(user_pt_regs, sp) as u64;
    reg64(reg_start)
}

const fn user_pc_reg64() -> u64 {
    let reg_start = std::mem::offset_of!(user_pt_regs, pc) as u64;
    reg64(reg_start)
}

const fn kvm_sp_el1_reg64() -> u64 {
    let reg_start = std::mem::offset_of!(kvm_regs, sp_el1) as u64;
    reg64(reg_start)
}

const fn kvm_elr_el1_reg64() -> u64 {
    let reg_start = std::mem::offset_of!(kvm_regs, elr_el1) as u64;
    reg64(reg_start)
}

#[bitfield(u16)]
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KvmSystemRegEncoding {
    #[bits(3)]
    pub op2: u8,
    #[bits(4)]
    pub crm: u8,
    #[bits(4)]
    pub crn: u8,
    #[bits(3)]
    pub op1: u8,
    #[bits(2)]
    pub op0: u8,
}

const fn sys_reg64(sr: SystemReg) -> u64 {
    let sr = KvmSystemRegEncoding::new()
        .with_op2(sr.0.op2())
        .with_crm(sr.0.crm())
        .with_crn(sr.0.crn())
        .with_op1(sr.0.op1())
        .with_op0(sr.0.op0());

    (sr.0 as u64) | (REG_ARM64_SYSREG_BASE | REG_SIZE_U64)
}

open_enum::open_enum! {
    enum KvmRegisterId: u64 {
        X0 = user_x_reg64(0),
        X1 = user_x_reg64(1),
        X2 = user_x_reg64(2),
        X3 = user_x_reg64(3),
        X4 = user_x_reg64(4),
        X5 = user_x_reg64(5),
        X6 = user_x_reg64(6),
        X7 = user_x_reg64(7),
        X8 = user_x_reg64(8),
        X9 = user_x_reg64(9),
        X10 = user_x_reg64(10),
        X11 = user_x_reg64(11),
        X12 = user_x_reg64(12),
        X13 = user_x_reg64(13),
        X14 = user_x_reg64(14),
        X15 = user_x_reg64(15),
        X16 = user_x_reg64(16),
        X17 = user_x_reg64(17),
        X18 = user_x_reg64(18),
        X19 = user_x_reg64(19),
        X20 = user_x_reg64(20),
        X21 = user_x_reg64(21),
        X22 = user_x_reg64(22),
        X23 = user_x_reg64(23),
        X24 = user_x_reg64(24),
        X25 = user_x_reg64(25),
        X26 = user_x_reg64(26),
        X27 = user_x_reg64(27),
        X28 = user_x_reg64(28),
        X29 = user_x_reg64(29),
        X30 = user_x_reg64(30),
        SP = user_sp_reg64(),
        PC = user_pc_reg64(),
        PSTATE = user_pstate_reg64(),
        SP_EL1 = kvm_sp_el1_reg64(),
        ELR_EL1 = kvm_elr_el1_reg64(),
        SYS_SCTLR_EL1 = sys_reg64(SystemReg::SCTLR),
        SYS_TTBR0_EL1 = sys_reg64(SystemReg::TTBR0_EL1),
        SYS_TTBR1_EL1 = sys_reg64(SystemReg::TTBR1_EL1),
        SYS_TCR_EL1 = sys_reg64(SystemReg::TCR_EL1),
        SYS_ESR_EL1 = sys_reg64(SystemReg::ESR_EL1),
        SYS_FAR_EL1 = sys_reg64(SystemReg::FAR_EL1),
        SYS_PAR_EL1 = sys_reg64(SystemReg::PAR_EL1),
        SYS_MAIR_EL1 = sys_reg64(SystemReg::MAIR_EL1),
        SYS_SPSR_EL1 = sys_reg64(SystemReg::SPSR_EL1),
        SYS_VBAR_EL1 = sys_reg64(SystemReg::VBAR),
    }
}

impl From<KvmRegisterId> for u64 {
    fn from(val: KvmRegisterId) -> Self {
        val.0
    }
}

#[derive(Debug, Inspect)]
pub struct KvmVpInner {
    #[inspect(skip)]
    needs_yield: NeedsYield,
    eval: AtomicBool,
    vp_info: Aarch64VpInfo,
}

impl KvmVpInner {
    pub fn set_eval(&self, value: bool, ordering: Ordering) {
        self.eval.store(value, ordering);
    }

    pub fn vp_info(&self) -> &Aarch64VpInfo {
        &self.vp_info
    }
}

#[derive(Debug)]
pub struct Kvm;

#[derive(InspectMut)]
pub struct KvmProcessor<'a> {
    #[inspect(skip)]
    partition: &'a KvmPartitionInner,
    #[inspect(flatten)]
    inner: &'a KvmVpInner,
    #[inspect(skip)]
    runner: kvm::VpRunner<'a>,
    #[inspect(skip)]
    kvm: kvm::Processor<'a>,
    vpindex: VpIndex,
    vmtime: &'a mut VmTimeAccess,
}

impl virt::vp::AccessVpState for &'_ mut KvmProcessor<'_> {
    type Error = KvmError;

    fn caps(&self) -> &PartitionCapabilities {
        &self.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<Registers, Self::Error> {
        let get_reg = |id: KvmRegisterId| -> Result<u64, KvmError> {
            // tracing::warn!("get_reg: {:?}", id);
            self.kvm.get_reg64(id.into()).map_err(KvmError::Kvm)
        };
        let regs = Registers {
            x0: get_reg(KvmRegisterId::X0)?,
            x1: get_reg(KvmRegisterId::X1)?,
            x2: get_reg(KvmRegisterId::X2)?,
            x3: get_reg(KvmRegisterId::X3)?,
            x4: get_reg(KvmRegisterId::X4)?,
            x5: get_reg(KvmRegisterId::X5)?,
            x6: get_reg(KvmRegisterId::X6)?,
            x7: get_reg(KvmRegisterId::X7)?,
            x8: get_reg(KvmRegisterId::X8)?,
            x9: get_reg(KvmRegisterId::X9)?,
            x10: get_reg(KvmRegisterId::X10)?,
            x11: get_reg(KvmRegisterId::X11)?,
            x12: get_reg(KvmRegisterId::X12)?,
            x13: get_reg(KvmRegisterId::X13)?,
            x14: get_reg(KvmRegisterId::X14)?,
            x15: get_reg(KvmRegisterId::X15)?,
            x16: get_reg(KvmRegisterId::X16)?,
            x17: get_reg(KvmRegisterId::X17)?,
            x18: get_reg(KvmRegisterId::X18)?,
            x19: get_reg(KvmRegisterId::X19)?,
            x20: get_reg(KvmRegisterId::X20)?,
            x21: get_reg(KvmRegisterId::X21)?,
            x22: get_reg(KvmRegisterId::X22)?,
            x23: get_reg(KvmRegisterId::X23)?,
            x24: get_reg(KvmRegisterId::X24)?,
            x25: get_reg(KvmRegisterId::X25)?,
            x26: get_reg(KvmRegisterId::X26)?,
            x27: get_reg(KvmRegisterId::X27)?,
            x28: get_reg(KvmRegisterId::X28)?,
            fp: get_reg(KvmRegisterId::X29)?,
            lr: get_reg(KvmRegisterId::X30)?,
            pc: get_reg(KvmRegisterId::PC)?,
            sp_el0: get_reg(KvmRegisterId::SP)?,
            sp_el1: get_reg(KvmRegisterId::SP_EL1)?,
            cpsr: get_reg(KvmRegisterId::PSTATE)?,
        };

        Ok(regs)
    }

    fn set_registers(&mut self, value: &Registers) -> Result<(), Self::Error> {
        let set_reg = |id: KvmRegisterId, value: u64| -> Result<(), KvmError> {
            // tracing::warn!("set_reg: {:?} = {:x}", id, value);
            self.kvm.set_reg64(id.into(), value).map_err(KvmError::Kvm)
        };

        set_reg(KvmRegisterId::X0, value.x0)?;
        set_reg(KvmRegisterId::X1, value.x1)?;
        set_reg(KvmRegisterId::X2, value.x2)?;
        set_reg(KvmRegisterId::X3, value.x3)?;
        set_reg(KvmRegisterId::X4, value.x4)?;
        set_reg(KvmRegisterId::X5, value.x5)?;
        set_reg(KvmRegisterId::X6, value.x6)?;
        set_reg(KvmRegisterId::X7, value.x7)?;
        set_reg(KvmRegisterId::X8, value.x8)?;
        set_reg(KvmRegisterId::X9, value.x9)?;
        set_reg(KvmRegisterId::X10, value.x10)?;
        set_reg(KvmRegisterId::X11, value.x11)?;
        set_reg(KvmRegisterId::X12, value.x12)?;
        set_reg(KvmRegisterId::X13, value.x13)?;
        set_reg(KvmRegisterId::X14, value.x14)?;
        set_reg(KvmRegisterId::X15, value.x15)?;
        set_reg(KvmRegisterId::X16, value.x16)?;
        set_reg(KvmRegisterId::X17, value.x17)?;
        set_reg(KvmRegisterId::X18, value.x18)?;
        set_reg(KvmRegisterId::X19, value.x19)?;
        set_reg(KvmRegisterId::X20, value.x20)?;
        set_reg(KvmRegisterId::X21, value.x21)?;
        set_reg(KvmRegisterId::X22, value.x22)?;
        set_reg(KvmRegisterId::X23, value.x23)?;
        set_reg(KvmRegisterId::X24, value.x24)?;
        set_reg(KvmRegisterId::X25, value.x25)?;
        set_reg(KvmRegisterId::X26, value.x26)?;
        set_reg(KvmRegisterId::X27, value.x27)?;
        set_reg(KvmRegisterId::X28, value.x28)?;
        set_reg(KvmRegisterId::X29, value.fp)?;
        set_reg(KvmRegisterId::X30, value.lr)?;
        set_reg(KvmRegisterId::SP, value.sp_el0)?;
        set_reg(KvmRegisterId::PC, value.pc)?;
        set_reg(KvmRegisterId::SP_EL1, value.sp_el1)?;
        set_reg(KvmRegisterId::PSTATE, value.cpsr)?;

        Ok(())
    }

    fn system_registers(&mut self) -> Result<SystemRegisters, Self::Error> {
        let get_reg = |id: KvmRegisterId| -> Result<u64, KvmError> {
            // tracing::warn!("get_sreg: {:?}({:#?})", id, id.0);
            self.kvm.get_reg64(id.into()).map_err(KvmError::Kvm)
        };

        let sregs = SystemRegisters {
            sctlr_el1: get_reg(KvmRegisterId::SYS_SCTLR_EL1)?,
            ttbr0_el1: get_reg(KvmRegisterId::SYS_TTBR0_EL1)?,
            ttbr1_el1: get_reg(KvmRegisterId::SYS_TTBR1_EL1)?,
            tcr_el1: get_reg(KvmRegisterId::SYS_TCR_EL1)?,
            esr_el1: get_reg(KvmRegisterId::SYS_ESR_EL1)?,
            far_el1: get_reg(KvmRegisterId::SYS_FAR_EL1)?,
            mair_el1: get_reg(KvmRegisterId::SYS_MAIR_EL1)?,
            elr_el1: get_reg(KvmRegisterId::ELR_EL1)?,
            vbar_el1: get_reg(KvmRegisterId::SYS_VBAR_EL1)?,
        };
        Ok(sregs)
    }

    fn set_system_registers(&mut self, value: &SystemRegisters) -> Result<(), Self::Error> {
        let set_reg = |id: KvmRegisterId, value: u64| -> Result<(), KvmError> {
            // tracing::warn!("set_sreg: {:?}({:#x}) = {:x}", id, id.0, value);
            self.kvm.set_reg64(id.into(), value).map_err(KvmError::Kvm)
        };

        set_reg(KvmRegisterId::SYS_SCTLR_EL1, value.sctlr_el1)?;
        set_reg(KvmRegisterId::SYS_TTBR0_EL1, value.ttbr0_el1)?;
        set_reg(KvmRegisterId::SYS_TTBR1_EL1, value.ttbr1_el1)?;
        set_reg(KvmRegisterId::SYS_TCR_EL1, value.tcr_el1)?;
        set_reg(KvmRegisterId::SYS_ESR_EL1, value.esr_el1)?;
        set_reg(KvmRegisterId::SYS_FAR_EL1, value.far_el1)?;
        set_reg(KvmRegisterId::SYS_MAIR_EL1, value.mair_el1)?;
        set_reg(KvmRegisterId::ELR_EL1, value.elr_el1)?;
        set_reg(KvmRegisterId::SYS_VBAR_EL1, value.vbar_el1)?;

        Ok(())
    }
}

impl virt::vm::AccessVmState for &KvmPartition {
    type Error = KvmError;

    fn caps(&self) -> &PartitionCapabilities {
        unimplemented!()
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        unimplemented!()
    }
}

impl virt::Processor for KvmProcessor<'_> {
    type StateAccess<'a>
        = &'a mut Self
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        _state: Option<&DebugState>,
    ) -> Result<(), <&mut Self as virt::vp::AccessVpState>::Error> {
        unimplemented!()
    }

    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        loop {
            self.inner.needs_yield.maybe_yield().await;
            stop.check()?;

            // Run the VP and handle exits until `evaluate_vp` is called or the
            // thread is otherwise interrupted.
            //
            // Don't break out of the loop while there is a pending exit so that
            // the register state is up-to-date for save.
            let mut pending_exit = false;
            loop {
                let exit = if self.inner.eval.load(Ordering::Relaxed) || stop.check().is_err() {
                    // Break out of the loop as soon as there is no pending exit.
                    if !pending_exit {
                        self.inner.eval.store(false, Ordering::Relaxed);
                        break;
                    }
                    // Complete the current exit.
                    self.runner.complete_exit()
                } else {
                    // Run the VP.
                    self.runner.run()
                };

                let exit = exit.map_err(|err| dev.fatal_error(KvmRunVpError::Run(err).into()))?;
                pending_exit = true;
                match exit {
                    kvm::Exit::Interrupted => {
                        pending_exit = false;
                    }
                    kvm::Exit::MmioWrite { address, data } => {
                        dev.write_mmio(self.vpindex, address, data).await
                    }
                    kvm::Exit::MmioRead { address, data } => {
                        dev.read_mmio(self.vpindex, address, data).await
                    }
                    kvm::Exit::Shutdown => {
                        return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                    }
                    kvm::Exit::Eoi { irq } => {
                        dev.handle_eoi(irq.into());
                    }
                    kvm::Exit::InternalError { error, .. } => {
                        return Err(dev.fatal_error(KvmRunVpError::InternalError(error).into()));
                    }
                    kvm::Exit::FailEntry {
                        hardware_entry_failure_reason,
                    } => {
                        tracing::error!(hardware_entry_failure_reason, "VP entry failed");
                        return Err(dev.fatal_error(KvmRunVpError::InvalidVpState.into()));
                    }
                    _ => panic!("unhandled exit: {:?}", exit),
                }
            }
        }
    }

    fn flush_async_requests(&mut self) {}

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        debug_assert_eq!(vtl, Vtl::Vtl0);
        self
    }
}

pub struct KvmProcessorBinder {
    partition: Arc<KvmPartitionInner>,
    vpindex: VpIndex,
    vmtime: VmTimeAccess,
}

impl virt::BindProcessor for KvmProcessorBinder {
    type Processor<'a> = KvmProcessor<'a>;
    type Error = KvmError;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        // FUTURE: create the vcpu here to get better NUMA affinity.

        let inner = &self.partition.vps[self.vpindex.index() as usize];
        let kvm = self.partition.kvm.vp(inner.vp_info.base.vp_index.index());
        let vp = KvmProcessor {
            partition: &self.partition,
            inner,
            runner: kvm.runner(),
            kvm,
            vpindex: self.vpindex,
            vmtime: &mut self.vmtime,
        };

        Ok(vp)
    }
}

pub struct KvmProtoPartition<'a> {
    vm: kvm::Partition,
    config: ProtoPartitionConfig<'a>,
    ipa_size: u8,
}

impl KvmProtoPartition<'_> {
    fn add_gicv3(&mut self) -> Result<(), KvmError> {
        // KVM requires the distributor and redistributor bases be _64KiB aligned_,
        // these ranges come from the OpenVMM MMIO gaps.
        const GIC_ALIGNMENT: u64 = 0x10000;
        let gic_dist_base: u64 = self.config.processor_topology.gic_distributor_base();
        let gic_redist_base: u64 = self.config.processor_topology.gic_redistributors_base();
        if !gic_dist_base.is_multiple_of(GIC_ALIGNMENT)
            || !gic_redist_base.is_multiple_of(GIC_ALIGNMENT)
        {
            return Err(KvmError::Misaligned);
        }

        const GIC_NR_IRQS: u32 = 64;
        const GIC_NR_SPIS: u32 = 32;

        let gicv3 = self
            .vm
            .create_device(kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3, 0)
            .map_err(kvm::Error::CreateDevice)?;

        // Set up the GICv3 device.

        // SAFETY: passing the right type for the attribute.
        unsafe {
            gicv3
                .set_device_attr::<u64>(
                    KVM_DEV_ARM_VGIC_GRP_ADDR,
                    KVM_VGIC_V3_ADDR_TYPE_REDIST,
                    &gic_redist_base,
                    0,
                )
                .map_err(kvm::Error::SetDeviceAttr)?;
        }

        // SAFETY: passing the right type for the attribute.
        unsafe {
            gicv3
                .set_device_attr::<u64>(
                    KVM_DEV_ARM_VGIC_GRP_ADDR,
                    KVM_VGIC_V3_ADDR_TYPE_DIST,
                    &gic_dist_base,
                    0,
                )
                .map_err(kvm::Error::SetDeviceAttr)?;
        }

        // SAFETY: passing the right type for the attribute.
        unsafe {
            gicv3
                .set_device_attr::<u32>(KVM_DEV_ARM_VGIC_GRP_NR_IRQS, 0, &GIC_NR_IRQS, 0)
                .map_err(kvm::Error::SetDeviceAttr)?;
        }

        // Initialize the GICv3 device.
        //
        // SAFETY: passing the right type for the attribute.
        unsafe {
            gicv3
                .set_device_attr::<()>(
                    KVM_DEV_ARM_VGIC_GRP_CTRL,
                    KVM_DEV_ARM_VGIC_CTRL_INIT,
                    &(),
                    0,
                )
                .map_err(kvm::Error::SetDeviceAttr)?;
        }

        // TODO: save gicv3 to a File to ensure it is cleaned up.
        std::mem::forget(gicv3);
        Ok(())
    }

    fn set_timer_ppis(&mut self, virt: u32, phys: u32) -> Result<(), KvmError> {
        // SAFETY: passing the right type for the attribute.
        unsafe {
            self.vm
                .vp(0)
                .set_device_attr::<u32>(
                    kvm::KVM_ARM_VCPU_TIMER_CTRL,
                    kvm::KVM_ARM_VCPU_TIMER_IRQ_VTIMER,
                    &virt,
                    0,
                )
                .map_err(kvm::Error::SetDeviceAttr)?;
        }

        // SAFETY: passing the right type for the attribute.
        unsafe {
            self.vm
                .vp(0)
                .set_device_attr::<u32>(
                    kvm::KVM_ARM_VCPU_TIMER_CTRL,
                    kvm::KVM_ARM_VCPU_TIMER_IRQ_PTIMER,
                    &phys,
                    0,
                )
                .map_err(kvm::Error::SetDeviceAttr)?;
        }
        Ok(())
    }
}

impl virt::ProtoPartition for KvmProtoPartition<'_> {
    type Error = KvmError;
    type Partition = KvmPartition;
    type ProcessorBinder = KvmProcessorBinder;

    fn max_physical_address_size(&self) -> u8 {
        self.ipa_size
    }

    fn build(
        mut self,
        config: virt::PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        for (vp_idx, _vp_info) in self.config.processor_topology.vps_arch().enumerate() {
            self.vm.add_vp(vp_idx as u32)?;
        }

        // TODO: Save the GICv3 FD to a File to ensure it is cleaned up.
        self.add_gicv3()?;

        // Use the Hyper-V timers instead of the ARM architectural ones. TODO:
        // make this configurable.
        self.set_timer_ppis(20, 19)?;

        let partition = KvmPartitionInner {
            kvm: self.vm,
            memory: Default::default(),
            hv1_enabled: self.config.hv_config.is_some(),
            gm: config.guest_memory.clone(),
            vps: self
                .config
                .processor_topology
                .vps_arch()
                .map(|vp_info| KvmVpInner {
                    vp_info,
                    needs_yield: NeedsYield::new(),
                    eval: false.into(),
                })
                .collect(),
            caps: PartitionCapabilities {},
        };

        let partition = KvmPartition {
            inner: Arc::new(partition),
        };

        kvm::init();

        let vps = self
            .config
            .processor_topology
            .vps()
            .map(|vp| KvmProcessorBinder {
                partition: partition.inner.clone(),
                vpindex: vp.vp_index,
                vmtime: self
                    .config
                    .vmtime
                    .access(format!("vp-{}", vp.vp_index.index())),
            })
            .collect::<Vec<_>>();

        Ok((partition, vps))
    }
}

impl virt::Partition for KvmPartition {
    fn supports_reset(
        &self,
    ) -> Option<&dyn virt::ResetPartition<Error = <Self as virt::Hv1>::Error>> {
        None
    }

    fn caps(&self) -> &PartitionCapabilities {
        &self.inner.caps
    }

    fn request_msi(&self, _vtl: Vtl, _request: virt::irqcon::MsiRequest) {
        tracelimit::warn_ratelimited!("msis not supported");
    }

    fn request_yield(&self, vp_index: VpIndex) {
        let vp = &self.inner.vps[vp_index.index() as usize];
        if vp.needs_yield.request_yield() {
            self.inner.evaluate_vp(vp_index);
        }
    }
}

impl virt::Hv1 for KvmPartition {
    type Error = KvmError;
    type Device = virt::UnimplementedDevice;

    fn reference_time_source(&self) -> Option<ReferenceTimeSource> {
        // TODO once Hyper-V enlightenments are implemented.
        None
    }

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        None
    }
}

const KVM_ARM_IRQ_TYPE_PPI: u32 = 2;
const KVM_ARM_IRQ_TYPE_SPI: u32 = 1;
const KVM_ARM_IRQ_TYPE_SHIFT: u32 = 24;
const KVM_ARM_IRQ_NUM_MASK: u32 = 0xffff;

const GIC_IRQ_BASE: u32 = 0x20;
const GIC_IRQ_MAX: u32 = 0x3fb;

impl virt::irqcon::ControlGic for KvmPartitionInner {
    fn set_spi_irq(&self, irq_id: u32, high: bool) {
        // tracing::warn!("set_spi_irq: irq_id={}", irq_id);
        debug_assert!(
            (GIC_IRQ_BASE..=GIC_IRQ_MAX).contains(&irq_id),
            "invalid irq_id"
        );

        let irqchip_irq =
            (KVM_ARM_IRQ_TYPE_SPI << KVM_ARM_IRQ_TYPE_SHIFT) | ((irq_id) & KVM_ARM_IRQ_NUM_MASK);

        self.kvm
            .irq_line(irqchip_irq, high)
            .expect("interrupt delivery failure");
    }
}

impl virt::Aarch64Partition for KvmPartition {
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic> {
        debug_assert!(vtl == Vtl::Vtl0);
        self.inner.clone()
    }
}

impl virt::PartitionAccessState for KvmPartition {
    type StateAccess<'a> = &'a KvmPartition;

    fn access_state(&self, vtl: Vtl) -> Self::StateAccess<'_> {
        debug_assert_eq!(vtl, Vtl::Vtl0);

        self
    }
}

impl virt::Synic for KvmPartition {
    fn post_message(&self, _vtl: Vtl, _vp: VpIndex, _sint: u8, _typ: u32, _payload: &[u8]) {
        unimplemented!()
    }

    fn new_guest_event_port(
        &self,
        _vtl: Vtl,
        _vp: u32,
        _sint: u8,
        _flag: u16,
    ) -> Box<dyn vmcore::synic::GuestEventPort> {
        unimplemented!()
    }

    fn prefer_os_events(&self) -> bool {
        unimplemented!()
    }
}

impl virt::Hypervisor for Kvm {
    type ProtoPartition<'a> = KvmProtoPartition<'a>;
    type Partition = KvmPartition;
    type Error = KvmError;

    fn is_available(&self) -> Result<bool, Self::Error> {
        match std::fs::metadata("/dev/kvm") {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(KvmError::AvailableCheck(err)),
        }
    }

    fn new_partition<'a>(
        &'a mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<Self::ProtoPartition<'a>, Self::Error> {
        if config.isolation.is_isolated() {
            return Err(KvmError::IsolationNotSupported);
        }

        let kvm = kvm::Kvm::new()?;

        if let Some(hv_config) = &config.hv_config {
            if hv_config.vtl2.is_some() {
                return Err(KvmError::Vtl2NotSupported);
            }
        }

        let vm = kvm.new_vm()?;

        Ok(KvmProtoPartition {
            vm,
            config,
            ipa_size: kvm.check_extension(KVM_CAP_ARM_VM_IPA_SIZE).unwrap_or(40) as u8,
        })
    }
}
