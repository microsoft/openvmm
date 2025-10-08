// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Emulator;
use super::InternalError;
use crate::Cpu;
use iced_x86::ConditionCode;
use iced_x86::Instruction;
use x86defs::RFlags;

impl<T: Cpu> Emulator<'_, T> {
    pub(super) async fn setcc(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let value = eval_cond(instr, self.cpu.rflags());
        self.write_op_0(instr, value as u64).await?;
        Ok(())
    }

    pub(super) async fn cmovcc(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        // CMOV always writes to the destination register. This may seem like a no-op on false conditions, but
        // actually can cause truncation when the destination is a 32-bit register.
        let src_op = if eval_cond(instr, self.cpu.rflags()) {
            1
        } else {
            0
        };
        let value = self.op_value(instr, src_op).await?;
        self.write_op_0(instr, value).await?;
        Ok(())
    }
}

fn eval_cond(instr: &Instruction, flags: RFlags) -> bool {
    match instr.condition_code() {
        ConditionCode::a => !flags.carry() && !flags.zero(),
        ConditionCode::ae => !flags.carry(),
        ConditionCode::b => flags.carry(),
        ConditionCode::be => flags.carry() || flags.zero(),
        ConditionCode::e => flags.zero(),
        ConditionCode::g => !flags.zero() && flags.sign() == flags.overflow(),
        ConditionCode::ge => flags.sign() == flags.overflow(),
        ConditionCode::l => flags.sign() != flags.overflow(),
        ConditionCode::le => flags.zero() || flags.sign() != flags.overflow(),
        ConditionCode::ne => !flags.zero(),
        ConditionCode::no => !flags.overflow(),
        ConditionCode::np => !flags.parity(),
        ConditionCode::ns => !flags.sign(),
        ConditionCode::o => flags.overflow(),
        ConditionCode::p => flags.parity(),
        ConditionCode::s => flags.sign(),
        ConditionCode::None => unreachable!(),
    }
}
