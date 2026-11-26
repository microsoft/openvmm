// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::IsolationType;
use crate::aarch64::Aarch64PartitionCapabilities;
use crate::state::HvRegisterState;
use crate::state::StateElement;
use crate::state::state_trait;
use aarch64defs::Cpsr64;
use aarch64defs::SctlrEl1;
use hvdef::HvArm64RegisterName;
use hvdef::HvRegisterValue;
use inspect::Inspect;
use mesh_protobuf::Protobuf;
use vm_topology::processor::aarch64::Aarch64VpInfo;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.aarch64")]
#[inspect(hex)]
pub struct Registers {
    #[mesh(1)]
    pub x0: u64,
    #[mesh(2)]
    pub x1: u64,
    #[mesh(3)]
    pub x2: u64,
    #[mesh(4)]
    pub x3: u64,
    #[mesh(5)]
    pub x4: u64,
    #[mesh(6)]
    pub x5: u64,
    #[mesh(7)]
    pub x6: u64,
    #[mesh(8)]
    pub x7: u64,
    #[mesh(9)]
    pub x8: u64,
    #[mesh(10)]
    pub x9: u64,
    #[mesh(11)]
    pub x10: u64,
    #[mesh(12)]
    pub x11: u64,
    #[mesh(13)]
    pub x12: u64,
    #[mesh(14)]
    pub x13: u64,
    #[mesh(15)]
    pub x14: u64,
    #[mesh(16)]
    pub x15: u64,
    #[mesh(17)]
    pub x16: u64,
    #[mesh(18)]
    pub x17: u64,
    #[mesh(19)]
    pub x18: u64,
    #[mesh(20)]
    pub x19: u64,
    #[mesh(21)]
    pub x20: u64,
    #[mesh(22)]
    pub x21: u64,
    #[mesh(23)]
    pub x22: u64,
    #[mesh(24)]
    pub x23: u64,
    #[mesh(25)]
    pub x24: u64,
    #[mesh(26)]
    pub x25: u64,
    #[mesh(27)]
    pub x26: u64,
    #[mesh(28)]
    pub x27: u64,
    #[mesh(29)]
    pub x28: u64,
    #[mesh(30)]
    pub fp: u64,
    #[mesh(31)]
    pub lr: u64,
    #[mesh(32)]
    pub sp_el0: u64,
    #[mesh(33)]
    pub sp_el1: u64,
    #[mesh(34)]
    pub pc: u64,
    #[mesh(35)]
    pub cpsr: u64,
}

impl HvRegisterState<HvArm64RegisterName, 35> for Registers {
    fn names(&self) -> &'static [HvArm64RegisterName; 35] {
        &[
            HvArm64RegisterName::X0,
            HvArm64RegisterName::X1,
            HvArm64RegisterName::X2,
            HvArm64RegisterName::X3,
            HvArm64RegisterName::X4,
            HvArm64RegisterName::X5,
            HvArm64RegisterName::X6,
            HvArm64RegisterName::X7,
            HvArm64RegisterName::X8,
            HvArm64RegisterName::X9,
            HvArm64RegisterName::X10,
            HvArm64RegisterName::X11,
            HvArm64RegisterName::X12,
            HvArm64RegisterName::X13,
            HvArm64RegisterName::X14,
            HvArm64RegisterName::X15,
            HvArm64RegisterName::X16,
            HvArm64RegisterName::X17,
            HvArm64RegisterName::X18,
            HvArm64RegisterName::X19,
            HvArm64RegisterName::X20,
            HvArm64RegisterName::X21,
            HvArm64RegisterName::X22,
            HvArm64RegisterName::X23,
            HvArm64RegisterName::X24,
            HvArm64RegisterName::X25,
            HvArm64RegisterName::X26,
            HvArm64RegisterName::X27,
            HvArm64RegisterName::X28,
            HvArm64RegisterName::XFp,
            HvArm64RegisterName::XLr,
            HvArm64RegisterName::XSpEl0,
            HvArm64RegisterName::XSpElx,
            HvArm64RegisterName::XPc,
            HvArm64RegisterName::Cpsr,
        ]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        let &Self {
            x0,
            x1,
            x2,
            x3,
            x4,
            x5,
            x6,
            x7,
            x8,
            x9,
            x10,
            x11,
            x12,
            x13,
            x14,
            x15,
            x16,
            x17,
            x18,
            x19,
            x20,
            x21,
            x22,
            x23,
            x24,
            x25,
            x26,
            x27,
            x28,
            fp,
            lr,
            sp_el0,
            sp_el1,
            pc,
            cpsr,
        } = self;
        for (dest, src) in it.zip([
            x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15, x16, x17, x18,
            x19, x20, x21, x22, x23, x24, x25, x26, x27, x28, fp, lr, sp_el0, sp_el1, pc, cpsr,
        ]) {
            *dest = src.into()
        }
    }

    fn set_values(&mut self, mut it: impl Iterator<Item = HvRegisterValue>) {
        let Self {
            x0,
            x1,
            x2,
            x3,
            x4,
            x5,
            x6,
            x7,
            x8,
            x9,
            x10,
            x11,
            x12,
            x13,
            x14,
            x15,
            x16,
            x17,
            x18,
            x19,
            x20,
            x21,
            x22,
            x23,
            x24,
            x25,
            x26,
            x27,
            x28,
            fp,
            lr,
            sp_el0,
            sp_el1,
            pc,
            cpsr,
        } = self;
        for (dest, src) in [
            x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15, x16, x17, x18,
            x19, x20, x21, x22, x23, x24, x25, x26, x27, x28, fp, lr, sp_el0, sp_el1, pc, cpsr,
        ]
        .into_iter()
        .zip(&mut it)
        {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<Aarch64PartitionCapabilities, Aarch64VpInfo> for Registers {
    fn is_present(_caps: &Aarch64PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(_caps: &Aarch64PartitionCapabilities, _vp_info: &Aarch64VpInfo) -> Self {
        Self {
            x0: 0,
            x1: 0,
            x2: 0,
            x3: 0,
            x4: 0,
            x5: 0,
            x6: 0,
            x7: 0,
            x8: 0,
            x9: 0,
            x10: 0,
            x11: 0,
            x12: 0,
            x13: 0,
            x14: 0,
            x15: 0,
            x16: 0,
            x17: 0,
            x18: 0,
            x19: 0,
            x20: 0,
            x21: 0,
            x22: 0,
            x23: 0,
            x24: 0,
            x25: 0,
            x26: 0,
            x27: 0,
            x28: 0,
            fp: 0,
            lr: 0,
            sp_el0: 0,
            sp_el1: 0,
            pc: 0,
            cpsr: Cpsr64::new()
                .with_sp(true)
                .with_el(1)
                .with_f(true)
                .with_i(true)
                .with_a(true)
                .with_d(true)
                .into(),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.aarch64")]
pub struct SystemRegisters {
    #[inspect(hex)]
    #[mesh(1)]
    pub sctlr_el1: u64,
    #[inspect(hex)]
    #[mesh(2)]
    pub ttbr0_el1: u64,
    #[inspect(hex)]
    #[mesh(3)]
    pub ttbr1_el1: u64,
    #[inspect(hex)]
    #[mesh(4)]
    pub tcr_el1: u64,
    #[inspect(hex)]
    #[mesh(5)]
    pub esr_el1: u64,
    #[inspect(hex)]
    #[mesh(6)]
    pub far_el1: u64,
    #[inspect(hex)]
    #[mesh(7)]
    pub mair_el1: u64,
    #[inspect(hex)]
    #[mesh(8)]
    pub elr_el1: u64,
    #[inspect(hex)]
    #[mesh(9)]
    pub vbar_el1: u64,
}

impl HvRegisterState<HvArm64RegisterName, 9> for SystemRegisters {
    fn names(&self) -> &'static [HvArm64RegisterName; 9] {
        &[
            HvArm64RegisterName::SctlrEl1,
            HvArm64RegisterName::Ttbr0El1,
            HvArm64RegisterName::Ttbr1El1,
            HvArm64RegisterName::TcrEl1,
            HvArm64RegisterName::EsrEl1,
            HvArm64RegisterName::FarEl1,
            HvArm64RegisterName::MairEl1,
            HvArm64RegisterName::ElrEl1,
            HvArm64RegisterName::VbarEl1,
        ]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        let &Self {
            sctlr_el1,
            ttbr0_el1,
            ttbr1_el1,
            tcr_el1,
            esr_el1,
            far_el1,
            mair_el1,
            elr_el1,
            vbar_el1,
        } = self;
        for (dest, src) in it.zip([
            sctlr_el1, ttbr0_el1, ttbr1_el1, tcr_el1, esr_el1, far_el1, mair_el1, elr_el1, vbar_el1,
        ]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        let Self {
            sctlr_el1,
            ttbr0_el1,
            ttbr1_el1,
            tcr_el1,
            esr_el1,
            far_el1,
            mair_el1,
            elr_el1,
            vbar_el1,
        } = self;
        for (src, dest) in it.zip([
            sctlr_el1, ttbr0_el1, ttbr1_el1, tcr_el1, esr_el1, far_el1, mair_el1, elr_el1, vbar_el1,
        ]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<Aarch64PartitionCapabilities, Aarch64VpInfo> for SystemRegisters {
    fn is_present(_caps: &Aarch64PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(caps: &Aarch64PartitionCapabilities, _vp: &Aarch64VpInfo) -> Self {
        // ITD and SED are RES1 when aarch32 is not supported. When aarch32 is
        // supported, they reset to 0.
        let no_aarch32 = !caps.supports_aarch32_el0;
        let sctlr_el1 = match caps.isolation {
            IsolationType::Cca => u64::from(
                SctlrEl1::new()
                    .with_sa(true)
                    .with_sa0(true)
                    .with_cp15ben(true)
                    .with_n_aa(true)
                    .with_eos(true)
                    .with_n_twi(true)
                    .with_n_twe(true)
                    .with_eis(true)
                    .with_span(true),
            ),
            _ => u64::from(
                SctlrEl1::new()
                    .with_eos(true)
                    .with_tscxt(true)
                    .with_eis(true)
                    .with_span(true)
                    .with_n_tlsmd(true)
                    .with_lsmaoe(true)
                    .with_itd(no_aarch32)
                    .with_sed(no_aarch32),
            ),
        };

        Self {
            sctlr_el1,
            ttbr0_el1: 0,
            ttbr1_el1: 0,
            tcr_el1: 0,
            esr_el1: 0,
            far_el1: 0,
            mair_el1: 0,
            elr_el1: 0,
            vbar_el1: 0,
        }
    }
}

state_trait! {
    "Per-VP state",
    AccessVpState,
    Aarch64PartitionCapabilities,
    Aarch64VpInfo,
    VpSavedState,
    "virt.aarch64",
    (1, "registers", registers, set_registers, Registers),
    (2, "system_registers", system_registers, set_system_registers, SystemRegisters),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aarch64::Aarch64InitialRegs;
    use aarch64defs::MpidrEl1;
    use test_with_tracing::test;
    use vm_topology::processor::VpIndex;
    use vm_topology::processor::VpInfo;

    fn caps(isolation: IsolationType, supports_aarch32_el0: bool) -> Aarch64PartitionCapabilities {
        Aarch64PartitionCapabilities {
            isolation,
            supports_aarch32_el0,
            vendor: aarch64defs::Vendor::ARM,
        }
    }

    fn bsp() -> Aarch64VpInfo {
        Aarch64VpInfo {
            base: VpInfo {
                vp_index: VpIndex::BSP,
                vnode: 0,
            },
            mpidr: MpidrEl1::new(),
            gicr: None,
            pmu_gsiv: None,
        }
    }

    fn values_of<T: HvRegisterState<HvArm64RegisterName, N>, const N: usize>(
        state: &T,
    ) -> [u64; N] {
        let mut values = [HvRegisterValue::from(0u64); N];
        state.get_values(values.iter_mut());
        values.map(|v| v.as_u64())
    }

    // Test: Write a distinct value to every general-purpose register through
    //       the register interface and read them back, checking the special
    //       registers by name.
    // Expected: Every value reads back unchanged, and FP, LR, SP_EL0, SP_EL1,
    //       PC, and CPSR land in their named fields.
    #[test]
    fn registers_round_trip_in_named_order() {
        let values: [u64; 35] = std::array::from_fn(|i| 0x1000 + i as u64);
        let mut regs = Registers::default();
        regs.set_values(values.map(HvRegisterValue::from).into_iter());

        assert_eq!(values_of(&regs), values);
        assert_eq!((regs.x0, regs.x28), (0x1000, 0x1000 + 28));
        assert_eq!((regs.fp, regs.lr), (0x1000 + 29, 0x1000 + 30));
        assert_eq!((regs.sp_el0, regs.sp_el1), (0x1000 + 31, 0x1000 + 32));
        assert_eq!((regs.pc, regs.cpsr), (0x1000 + 33, 0x1000 + 34));
        let names = regs.names();
        assert_eq!(names[29], HvArm64RegisterName::XFp);
        assert_eq!(names[32], HvArm64RegisterName::XSpElx);
        assert_eq!(names[34], HvArm64RegisterName::Cpsr);
    }

    // Test: Write a distinct value to every EL1 system register through the
    //       register interface and read them back.
    // Expected: Every value reads back unchanged in the order of names(), and
    //       each named register holds its own field.
    #[test]
    fn system_registers_round_trip_in_named_order() {
        let values: [u64; 9] = std::array::from_fn(|i| 0x2000 + i as u64);
        let mut regs = SystemRegisters::default();
        regs.set_values(values.map(HvRegisterValue::from).into_iter());

        assert_eq!(values_of(&regs), values);
        assert_eq!(
            regs,
            SystemRegisters {
                sctlr_el1: 0x2000,
                ttbr0_el1: 0x2001,
                ttbr1_el1: 0x2002,
                tcr_el1: 0x2003,
                esr_el1: 0x2004,
                far_el1: 0x2005,
                mair_el1: 0x2006,
                elr_el1: 0x2007,
                vbar_el1: 0x2008,
            }
        );
        assert_eq!(regs.names()[8], HvArm64RegisterName::VbarEl1);
    }

    // Test: Compute the boot register state for the BSP.
    // Expected: Every general-purpose register, stack pointer, and PC is zero,
    //       and CPSR is EL1 using SP_EL1 with D, A, I, and F masked (0x3c5).
    #[test]
    fn registers_reset_to_el1h_with_interrupts_masked() {
        let regs = Registers::at_reset(&caps(IsolationType::None, false), &bsp());

        assert_eq!(
            regs,
            Registers {
                cpsr: 0x3c5,
                ..Default::default()
            }
        );
    }

    // Test: Compute the reset SCTLR_EL1 for a non-isolated processor without
    //       and with AArch32 EL0 support, and for a CCA realm.
    // Expected: Non-isolated sets EOS, TSCXT, EIS, SPAN, nTLSMD, and LSMAOE,
    //       plus ITD and SED (RES1) only without AArch32; CCA sets SA, SA0,
    //       CP15BEN, nAA, EOS, nTWI, nTWE, EIS, and SPAN. Every other system
    //       register is zero.
    #[test]
    fn system_registers_reset_by_isolation_and_aarch32() {
        let reset =
            |isolation, aarch32| SystemRegisters::at_reset(&caps(isolation, aarch32), &bsp());

        assert_eq!(reset(IsolationType::None, false).sctlr_el1, 0x30d0_0980);
        assert_eq!(reset(IsolationType::None, true).sctlr_el1, 0x30d0_0800);
        assert_eq!(reset(IsolationType::Cca, false).sctlr_el1, 0x00c5_0878);
        assert_eq!(
            reset(IsolationType::None, false),
            SystemRegisters {
                sctlr_el1: 0x30d0_0980,
                ..Default::default()
            }
        );

        let initial = Aarch64InitialRegs::at_reset(&caps(IsolationType::None, true), &bsp());
        assert_eq!(initial.registers.cpsr, 0x3c5);
        assert_eq!(initial.system_registers.sctlr_el1, 0x30d0_0800);
    }
}
