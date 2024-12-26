// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::AlignmentMode;
use super::Emulator;
use super::InternalError;
use crate::Cpu;
use iced_x86::Instruction;
use iced_x86::Register;

impl<T: Cpu> Emulator<'_, T> {
    // cmpxchg8/16 rm
    pub(super) async fn cmpxchg8_16(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let op_size = instr.memory_size().size() * 8;

        let left: u128 = match op_size {
            64 => {
                self.read_memory_op::<u64>(instr, 0, AlignmentMode::Standard)
                    .await? as u128
            }
            128 => {
                self.read_memory_op(instr, 0, AlignmentMode::Standard)
                    .await?
            }
            _ => unreachable!(),
        };

        let cmp_val = self.get_combined_value(op_size, RegisterPair::DxAx);

        if left == cmp_val {
            let new_val = self.get_combined_value(op_size, RegisterPair::CxBx);
            match op_size {
                64 => {
                    self.compare_if_locked_and_write_memory_op(
                        instr,
                        0,
                        AlignmentMode::Standard,
                        left as u64,
                        new_val as u64,
                    )
                    .await
                }
                128 => {
                    self.compare_if_locked_and_write_memory_op(
                        instr,
                        0,
                        AlignmentMode::Standard,
                        left,
                        new_val,
                    )
                    .await
                }
                _ => unreachable!(),
            }?;
            let mut rflags = self.cpu.rflags();
            rflags.set_zero(true);
            self.cpu.set_rflags(rflags);
        } else {
            let mut rflags = self.cpu.rflags();
            rflags.set_zero(false);
            let high_val = (left >> (op_size / 2)) as u64;
            let low_val = (left & !(u128::MAX << (op_size / 2))) as u64;
            let (high_reg, low_reg) = RegisterPair::DxAx.to_registers(op_size);
            self.cpu.set_gp(high_reg.into(), high_val);
            self.cpu.set_gp(low_reg.into(), low_val);
            self.cpu.set_rflags(rflags);
        }

        Ok(())
    }

    fn get_combined_value(&mut self, op_size: usize, pair: RegisterPair) -> u128 {
        let (high_reg, low_reg) = pair.to_registers(op_size);
        let high_val = self.cpu.gp(high_reg.into()) as u128;
        let low_val = self.cpu.gp(low_reg.into()) as u128;
        (high_val << (op_size / 2)) | low_val
    }
}

#[derive(Clone, Copy)]
enum RegisterPair {
    DxAx,
    CxBx,
}

impl RegisterPair {
    fn to_registers(self, op_size: usize) -> (Register, Register) {
        match (self, op_size) {
            (RegisterPair::CxBx, 64) => (Register::ECX, Register::EBX),
            (RegisterPair::CxBx, 128) => (Register::RCX, Register::RBX),
            (RegisterPair::DxAx, 64) => (Register::EDX, Register::EAX),
            (RegisterPair::DxAx, 128) => (Register::RDX, Register::RAX),
            _ => unreachable!(),
        }
    }
}
