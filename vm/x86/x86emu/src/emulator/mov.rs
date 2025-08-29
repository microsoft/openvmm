// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::AlignmentMode;
use super::Emulator;
use super::InternalError;
use crate::Cpu;
use crate::Segment;
use iced_x86::Instruction;
use iced_x86::OpKind;

impl<T: Cpu> Emulator<'_, T> {
    pub(super) async fn mov(&mut self, instr: &Instruction) -> Result<(), InternalError<T::Error>> {
        let value = self.op_value(instr, 1).await?;
        self.write_op_0(instr, value).await?;
        Ok(())
    }

    pub(super) async fn movsx(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let value = self.op_value_sign_extend(instr, 1).await?;
        self.write_op_0(instr, value as u64).await?;
        Ok(())
    }

    pub(super) async fn mov_sse(
        &mut self,
        instr: &Instruction,
        alignment: AlignmentMode,
    ) -> Result<(), InternalError<T::Error>> {
        let value = match instr.op1_kind() {
            OpKind::Memory => self.read_memory_op(instr, 1, alignment).await?,
            OpKind::Register => {
                let reg = instr.op1_register();
                assert!(reg.is_xmm());
                self.cpu.xmm(reg.number())
            }
            _ => Err(self.unsupported_instruction(instr))?,
        };

        match instr.op0_kind() {
            OpKind::Memory => self.write_memory_op(instr, 0, alignment, value).await?,
            OpKind::Register => {
                let reg = instr.op0_register();
                assert!(reg.is_xmm());
                let xmm_index = reg.number();
                self.cpu.set_xmm(xmm_index, value)
            }
            _ => Err(self.unsupported_instruction(instr))?,
        };

        Ok(())
    }

    pub(super) async fn movdir64b(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let mut buffer = [0; 64];
        let src = self.memory_op_offset(instr, 1);
        let dst = self.cpu.gp(instr.op0_register().into());

        self.read_memory(
            instr.memory_segment().into(),
            src,
            AlignmentMode::Unaligned,
            &mut buffer,
        )
        .await?;

        self.write_memory(Segment::ES, dst, AlignmentMode::Aligned(64), &buffer)
            .await?;

        Ok(())
    }
}
