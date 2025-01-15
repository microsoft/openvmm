// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::instruction;
use super::AlignmentMode;
use super::Emulator;
use super::InternalError;
use crate::Cpu;
use iced_x86::Instruction;

impl<T: Cpu> Emulator<'_, T> {
    // bt/btc/btr/bts m, r/imm
    pub(super) async fn bt_m<Op: BitOp>(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let op_size = instr.memory_size().size() as u8;

        let bit_offset = match instr.op1_kind() {
            // When in the register form, the offset is treated as a signed value
            iced_x86::OpKind::Register => self.op_value_sign_extend(instr, 1).await?,
            // When in the immediate form, the offset wraps around the operand size
            iced_x86::OpKind::Immediate8 => (instr.immediate8() % (op_size * 8)).into(),
            _ => Err(self.unsupported_instruction(instr))?,
        };

        let address_size = instruction::address_size(instr);

        let op_size = op_size as i64;
        let bit_base = self.memory_op_offset(instr, 0);
        let address_mask = u64::MAX >> (64 - address_size * 8);
        let address = bit_base.wrapping_add_signed(op_size * bit_offset.div_euclid(op_size * 8))
            & address_mask;

        let mut data = [0; 8];
        self.read_memory(
            instr.memory_segment(),
            address,
            AlignmentMode::Standard,
            &mut data[..op_size as usize],
        )
        .await?;
        let val = u64::from_le_bytes(data);

        let mask = 1 << (bit_offset.rem_euclid(op_size * 8));

        if Op::UPDATES_RESULT {
            let new_val = (Op::op(val) & mask) | (val & !mask);

            if instr.has_lock_prefix() {
                if !self
                    .compare_and_write_memory(
                        instr.memory_segment(),
                        address,
                        AlignmentMode::Standard,
                        &val.to_le_bytes()[..op_size as usize],
                        &new_val.to_le_bytes()[..op_size as usize],
                    )
                    .await?
                {
                    return Err(InternalError::Retry);
                }
            } else {
                self.write_memory(
                    instr.memory_segment(),
                    address,
                    AlignmentMode::Standard,
                    &new_val.to_le_bytes()[..op_size as usize],
                )
                .await?;
            }
        }

        let carry = val & mask != 0;
        let mut rflags = self.cpu.rflags();
        rflags.set_carry(carry);
        self.cpu.set_rflags(rflags);

        Ok(())
    }
}

/// Trait for unary bit ops (bt, bts, etc.)
pub(super) trait BitOp {
    const UPDATES_RESULT: bool = true;
    fn op(value: u64) -> u64;
}

pub(super) struct ResetOp;
impl BitOp for ResetOp {
    const UPDATES_RESULT: bool = true;
    fn op(_value: u64) -> u64 {
        0
    }
}

pub(super) struct SetOp;
impl BitOp for SetOp {
    const UPDATES_RESULT: bool = true;
    fn op(_value: u64) -> u64 {
        !0
    }
}

pub(super) struct ComplementOp;
impl BitOp for ComplementOp {
    const UPDATES_RESULT: bool = true;
    fn op(value: u64) -> u64 {
        !value
    }
}

pub(super) struct TestOp;
impl BitOp for TestOp {
    const UPDATES_RESULT: bool = false;
    fn op(_value: u64) -> u64 {
        unreachable!()
    }
}
