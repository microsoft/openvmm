// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VP state handling.

use crate::Error;
use crate::HvfProcessor;
use crate::abi;
use hvdef::HvArm64RegisterName;
use virt::aarch64::Aarch64PartitionCapabilities;
use virt::aarch64::vp::AccessVpState;
use virt::state::HvRegisterState;

enum Reg {
    Reg(abi::HvReg),
    SysReg(abi::HvSysReg),
}

fn hv_to_hvf(name: HvArm64RegisterName) -> Option<Reg> {
    let v = match name {
        HvArm64RegisterName::X0 => Reg::Reg(abi::HvReg::X0),
        HvArm64RegisterName::X1 => Reg::Reg(abi::HvReg::X1),
        HvArm64RegisterName::X2 => Reg::Reg(abi::HvReg::X2),
        HvArm64RegisterName::X3 => Reg::Reg(abi::HvReg::X3),
        HvArm64RegisterName::X4 => Reg::Reg(abi::HvReg::X4),
        HvArm64RegisterName::X5 => Reg::Reg(abi::HvReg::X5),
        HvArm64RegisterName::X6 => Reg::Reg(abi::HvReg::X6),
        HvArm64RegisterName::X7 => Reg::Reg(abi::HvReg::X7),
        HvArm64RegisterName::X8 => Reg::Reg(abi::HvReg::X8),
        HvArm64RegisterName::X9 => Reg::Reg(abi::HvReg::X9),
        HvArm64RegisterName::X10 => Reg::Reg(abi::HvReg::X10),
        HvArm64RegisterName::X11 => Reg::Reg(abi::HvReg::X11),
        HvArm64RegisterName::X12 => Reg::Reg(abi::HvReg::X12),
        HvArm64RegisterName::X13 => Reg::Reg(abi::HvReg::X13),
        HvArm64RegisterName::X14 => Reg::Reg(abi::HvReg::X14),
        HvArm64RegisterName::X15 => Reg::Reg(abi::HvReg::X15),
        HvArm64RegisterName::X16 => Reg::Reg(abi::HvReg::X16),
        HvArm64RegisterName::X17 => Reg::Reg(abi::HvReg::X17),
        HvArm64RegisterName::X18 => Reg::Reg(abi::HvReg::X18),
        HvArm64RegisterName::X19 => Reg::Reg(abi::HvReg::X19),
        HvArm64RegisterName::X20 => Reg::Reg(abi::HvReg::X20),
        HvArm64RegisterName::X21 => Reg::Reg(abi::HvReg::X21),
        HvArm64RegisterName::X22 => Reg::Reg(abi::HvReg::X22),
        HvArm64RegisterName::X23 => Reg::Reg(abi::HvReg::X23),
        HvArm64RegisterName::X24 => Reg::Reg(abi::HvReg::X24),
        HvArm64RegisterName::X25 => Reg::Reg(abi::HvReg::X25),
        HvArm64RegisterName::X26 => Reg::Reg(abi::HvReg::X26),
        HvArm64RegisterName::X27 => Reg::Reg(abi::HvReg::X27),
        HvArm64RegisterName::X28 => Reg::Reg(abi::HvReg::X28),
        HvArm64RegisterName::XFp => Reg::Reg(abi::HvReg::FP),
        HvArm64RegisterName::XLr => Reg::Reg(abi::HvReg::LR),
        HvArm64RegisterName::XSpEl0 => Reg::SysReg(abi::HvSysReg::SP_EL0),
        HvArm64RegisterName::XSpElx => Reg::SysReg(abi::HvSysReg::SP_EL1),
        HvArm64RegisterName::XPc => Reg::Reg(abi::HvReg::PC),
        HvArm64RegisterName::Cpsr => Reg::Reg(abi::HvReg::CPSR),
        HvArm64RegisterName::SctlrEl1 => Reg::SysReg(abi::HvSysReg::SCTLR_EL1),
        HvArm64RegisterName::Ttbr0El1 => Reg::SysReg(abi::HvSysReg::TTBR0_EL1),
        HvArm64RegisterName::Ttbr1El1 => Reg::SysReg(abi::HvSysReg::TTBR1_EL1),
        HvArm64RegisterName::TcrEl1 => Reg::SysReg(abi::HvSysReg::TCR_EL1),
        HvArm64RegisterName::EsrEl1 => Reg::SysReg(abi::HvSysReg::ESR_EL1),
        HvArm64RegisterName::FarEl1 => Reg::SysReg(abi::HvSysReg::FAR_EL1),
        HvArm64RegisterName::MairEl1 => Reg::SysReg(abi::HvSysReg::MAIR_EL1),
        HvArm64RegisterName::ElrEl1 => Reg::SysReg(abi::HvSysReg::ELR_EL1),
        HvArm64RegisterName::VbarEl1 => Reg::SysReg(abi::HvSysReg::VBAR_EL1),
        _ => {
            tracing::error!(?name, "Unsupported register name translation");
            return None;
        }
    };
    Some(v)
}

pub struct HvfVpStateAccess<'a, 'b> {
    pub(crate) processor: &'a mut HvfProcessor<'b>,
}

impl HvfVpStateAccess<'_, '_> {
    pub(crate) fn set_register_state<T, const N: usize>(&mut self, value: &T) -> Result<(), Error>
    where
        T: HvRegisterState<HvArm64RegisterName, N>,
    {
        let mut values = [0u32.into(); N];
        value.get_values(values.iter_mut());
        for (&name, value) in value.names().iter().zip(values) {
            match hv_to_hvf(name).unwrap() {
                Reg::Reg(reg) => self.processor.vcpu.set_reg(reg, value.as_u64())?,
                Reg::SysReg(reg) => self.processor.vcpu.set_sys_reg(reg, value.as_u64())?,
            }
        }

        Ok(())
    }

    pub(crate) fn get_register_state<T, const N: usize>(&mut self) -> Result<T, Error>
    where
        T: HvRegisterState<HvArm64RegisterName, N>,
    {
        let mut value = T::default();
        let mut values = [0u64; N];
        for (&name, value) in value.names().iter().zip(&mut values) {
            *value = match hv_to_hvf(name).unwrap() {
                Reg::Reg(reg) => self.processor.vcpu.reg(reg)?,
                Reg::SysReg(reg) => self.processor.vcpu.sys_reg(reg)?,
            };
        }

        value.set_values(values.into_iter().map(|v| v.into()));
        Ok(value)
    }
}

impl AccessVpState for HvfVpStateAccess<'_, '_> {
    type Error = Error;

    fn caps(&self) -> &Aarch64PartitionCapabilities {
        &self.processor.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<virt::aarch64::vp::Registers, Self::Error> {
        self.get_register_state()
    }

    fn set_registers(&mut self, value: &virt::aarch64::vp::Registers) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn system_registers(&mut self) -> Result<virt::vp::SystemRegisters, Self::Error> {
        self.get_register_state()
    }

    fn set_system_registers(
        &mut self,
        value: &virt::vp::SystemRegisters,
    ) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }
}
