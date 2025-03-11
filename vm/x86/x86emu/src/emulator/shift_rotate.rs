// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::rflags::update_flags_szp;
use super::Emulator;
use super::InternalError;
use crate::Cpu;
use iced_x86::Instruction;
use x86defs::RFlags;

const LSB_MASK: u64 = 0x1;

impl<T: Cpu> Emulator<'_, T> {
    // shr/shl/sal/rol/ror/rcl/rcr rm, 1/imm/cl
    pub(super) async fn shift_sign_unextended<Op: ShiftingOp>(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value(instr, 0).await?;
        self.shift::<Op>(instr, left, 1).await
    }

    // sar rm, 1/imm/cl
    pub(super) async fn shift_arithmetic_right(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value_sign_extend(instr, 0).await?;
        self.shift::<SarOp>(instr, left as u64, 1).await
    }

    // shld rm, r, imm/cl
    pub(super) async fn shld(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value(instr, 0).await?;
        self.shift::<ShldOp>(instr, left, 2).await
    }

    // shld rm, r, imm/cl
    pub(super) async fn shrd(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value(instr, 0).await?;
        self.shift::<ShrdOp>(instr, left, 2).await
    }

    // performs a shift or rotate operation
    async fn shift<Op: ShiftingOp>(
        &mut self,
        instr: &Instruction,
        left: u64,
        count_op: u32,
    ) -> Result<(), InternalError<T::Error>> {
        let operand_size = instr.memory_size().size();
        let operand_bit_size = operand_size as u32 * 8;

        let masked_count = shift_count(self.op_value(instr, count_op).await?, operand_bit_size);
        let count = Op::mod_count(masked_count, operand_bit_size);

        let right = self.op_value(instr, 1).await?;
        let mut rflags = self.cpu.rflags();

        if count == 0 {
            if Op::ZERO_SHIFT_UPDATES_CARRY && masked_count != 0 {
                // left is unchanged, so left is the result
                rflags.set_carry(Op::carry_flag(left, right, left, count, operand_bit_size));
                self.cpu.set_rflags(rflags);
            }
            // flags unchanged
            return Ok(());
        }
        let result = Op::op(left, right, count, rflags, operand_bit_size);
        let carry = Op::carry_flag(left, right, result, count, operand_bit_size);

        self.write_op_0(instr, result).await?;

        if Op::UPDATE_SZP {
            update_flags_szp(&mut rflags, operand_size, result);
        }

        rflags.set_carry(carry);

        if (Op::MASKED_COUNT_UPDATES_OF && masked_count == 1)
            || (!Op::MASKED_COUNT_UPDATES_OF && count == 1)
        {
            rflags.set_overflow(Op::overflow_flag(left, result, carry, operand_bit_size));
        }

        self.cpu.set_rflags(rflags);

        Ok(())
    }
}

/// Trait for rotate and shift operations
pub(super) trait ShiftingOp {
    /// Whether sign, zero, and parity flags are updated
    const UPDATE_SZP: bool;
    /// Whether the carry flag may still be updated if no shift was performed,
    /// based on the masked count
    const ZERO_SHIFT_UPDATES_CARRY: bool;
    /// Whether the overflow flag is updated based on the masked count
    const MASKED_COUNT_UPDATES_OF: bool;
    /// Actual operation
    fn op(left: u64, right: u64, count: u32, flags: RFlags, operand_bit_size: u32) -> u64;
    /// Modulates the count
    fn mod_count(masked_count: u32, operand_bit_size: u32) -> u32;
    /// calculates the carry flag
    fn carry_flag(left: u64, right: u64, result: u64, count: u32, operand_bit_size: u32) -> bool;
    /// calculates the overflow flag
    fn overflow_flag(left: u64, result: u64, carry_flag: bool, operand_bit_size: u32) -> bool;
}

pub(super) struct SxlOp {}
impl ShiftingOp for SxlOp {
    const UPDATE_SZP: bool = true;
    const ZERO_SHIFT_UPDATES_CARRY: bool = false;
    const MASKED_COUNT_UPDATES_OF: bool = false;
    fn op(left: u64, _right: u64, count: u32, _flags: RFlags, _operand_bit_size: u32) -> u64 {
        left << count
    }

    fn mod_count(masked_count: u32, _operand_bit_size: u32) -> u32 {
        masked_count
    }

    fn carry_flag(left: u64, _right: u64, _result: u64, count: u32, operand_bit_size: u32) -> bool {
        ((left << (count - 1)) & msb_mask(operand_bit_size)) != 0
    }

    fn overflow_flag(_left: u64, result: u64, carry_flag: bool, operand_bit_size: u32) -> bool {
        ((msb_mask(operand_bit_size) & result) != 0) ^ carry_flag
    }
}

struct SarOp {}
impl ShiftingOp for SarOp {
    const UPDATE_SZP: bool = true;
    const ZERO_SHIFT_UPDATES_CARRY: bool = false;
    const MASKED_COUNT_UPDATES_OF: bool = false;
    fn op(left: u64, _right: u64, count: u32, _flags: RFlags, _operand_bit_size: u32) -> u64 {
        ((left as i64) >> count) as u64
    }

    fn mod_count(masked_count: u32, _operand_bit_size: u32) -> u32 {
        masked_count
    }

    fn carry_flag(
        left: u64,
        _right: u64,
        _result: u64,
        count: u32,
        _operand_bit_size: u32,
    ) -> bool {
        (left >> (count - 1) & LSB_MASK) != 0
    }

    fn overflow_flag(_left: u64, _result: u64, _carry_flag: bool, _operand_bit_size: u32) -> bool {
        false
    }
}

pub(super) struct ShrOp {}
impl ShiftingOp for ShrOp {
    const UPDATE_SZP: bool = true;
    const ZERO_SHIFT_UPDATES_CARRY: bool = false;
    const MASKED_COUNT_UPDATES_OF: bool = false;
    fn op(left: u64, _right: u64, count: u32, _flags: RFlags, _operand_bit_size: u32) -> u64 {
        left >> count
    }

    fn mod_count(masked_count: u32, _operand_bit_size: u32) -> u32 {
        masked_count
    }

    fn carry_flag(
        left: u64,
        _right: u64,
        _result: u64,
        count: u32,
        _operand_bit_size: u32,
    ) -> bool {
        (left >> (count - 1) & LSB_MASK) != 0
    }

    fn overflow_flag(left: u64, _result: u64, _carry_flag: bool, operand_bit_size: u32) -> bool {
        (msb_mask(operand_bit_size) & left) != 0
    }
}

struct ShldOp {}
impl ShiftingOp for ShldOp {
    const UPDATE_SZP: bool = true;
    const ZERO_SHIFT_UPDATES_CARRY: bool = false;
    const MASKED_COUNT_UPDATES_OF: bool = false;
    fn op(left: u64, right: u64, count: u32, _flags: RFlags, operand_bit_size: u32) -> u64 {
        // When operating on 16 bit operands it is possible for count to be greater than operand_bit_size.
        // The results in this case are undefined, but real hardware appears to treat this as an oversized rotate.
        if operand_bit_size == 16 && count > 16 {
            let combined: u32 = ((left as u32) << 16) | (right as u32);
            (combined.rotate_left(count - 16) as u16).into()
        } else {
            (left << count) | (right >> (operand_bit_size - count))
        }
    }

    fn mod_count(masked_count: u32, _operand_bit_size: u32) -> u32 {
        masked_count
    }

    fn carry_flag(left: u64, right: u64, _result: u64, count: u32, operand_bit_size: u32) -> bool {
        // When operating on 16 bit operands it is possible for count to be greater than operand_bit_size.
        // The results in this case are undefined, but real hardware appears to treat this as an oversized rotate.
        if operand_bit_size == 16 && count > 16 {
            ((right >> (operand_bit_size - (count - 16))) & LSB_MASK) != 0
        } else {
            ((left >> (operand_bit_size - count)) & LSB_MASK) != 0
        }
    }

    fn overflow_flag(left: u64, result: u64, _carry_flag: bool, operand_bit_size: u32) -> bool {
        (msb_mask(operand_bit_size) & (left ^ result)) != 0
    }
}

struct ShrdOp {}
impl ShiftingOp for ShrdOp {
    const UPDATE_SZP: bool = true;
    const ZERO_SHIFT_UPDATES_CARRY: bool = false;
    const MASKED_COUNT_UPDATES_OF: bool = false;
    fn op(left: u64, right: u64, count: u32, _flags: RFlags, operand_bit_size: u32) -> u64 {
        // When operating on 16 bit operands it is possible for count to be greater than operand_bit_size.
        // The results in this case are undefined, but real hardware appears to treat this as an oversized rotate.
        if operand_bit_size == 16 && count > 16 {
            let combined: u32 = ((left as u32) << 16) | (right as u32);
            (combined.rotate_right(count - 16) as u16).into()
        } else {
            (left >> count) | (right << (operand_bit_size - count))
        }
    }

    fn mod_count(masked_count: u32, _operand_bit_size: u32) -> u32 {
        masked_count
    }

    fn carry_flag(left: u64, right: u64, _result: u64, count: u32, operand_bit_size: u32) -> bool {
        // When operating on 16 bit operands it is possible for count to be greater than operand_bit_size.
        // The results in this case are undefined, but real hardware appears to treat this as an oversized rotate.
        if operand_bit_size == 16 && count > 16 {
            ((right >> (count - 17)) & LSB_MASK) != 0
        } else {
            ((left >> (count - 1)) & LSB_MASK) != 0
        }
    }

    fn overflow_flag(_left: u64, result: u64, carry_flag: bool, operand_bit_size: u32) -> bool {
        ((msb_mask(operand_bit_size) & result) != 0) ^ carry_flag
    }
}

pub(super) struct RolOp {}
impl ShiftingOp for RolOp {
    const UPDATE_SZP: bool = false;
    const ZERO_SHIFT_UPDATES_CARRY: bool = true;
    const MASKED_COUNT_UPDATES_OF: bool = true;
    fn op(left: u64, _right: u64, count: u32, _flags: RFlags, operand_bit_size: u32) -> u64 {
        let result = (left << count) | (left >> (operand_bit_size - count));
        sign_extend(result, operand_bit_size)
    }

    fn carry_flag(
        _left: u64,
        _right: u64,
        result: u64,
        _count: u32,
        _operand_bit_size: u32,
    ) -> bool {
        (result & LSB_MASK) != 0
    }

    fn mod_count(masked_count: u32, operand_bit_size: u32) -> u32 {
        rox_mod_count(masked_count, operand_bit_size)
    }

    fn overflow_flag(_left: u64, result: u64, carry_flag: bool, operand_bit_size: u32) -> bool {
        rotate_left_overflow(result, carry_flag, operand_bit_size)
    }
}

pub(super) struct RorOp {}
impl ShiftingOp for RorOp {
    const UPDATE_SZP: bool = false;
    const ZERO_SHIFT_UPDATES_CARRY: bool = true;
    const MASKED_COUNT_UPDATES_OF: bool = true;
    fn op(left: u64, _right: u64, count: u32, _flags: RFlags, operand_bit_size: u32) -> u64 {
        let result = (left >> count) | (left << (operand_bit_size - count));
        sign_extend(result, operand_bit_size)
    }

    fn carry_flag(
        _left: u64,
        _right: u64,
        result: u64,
        _count: u32,
        operand_bit_size: u32,
    ) -> bool {
        (result & msb_mask(operand_bit_size)) != 0
    }

    fn mod_count(masked_count: u32, operand_bit_size: u32) -> u32 {
        rox_mod_count(masked_count, operand_bit_size)
    }

    fn overflow_flag(_left: u64, result: u64, carry_flag: bool, operand_bit_size: u32) -> bool {
        rotate_right_overflow(result, carry_flag, operand_bit_size)
    }
}

pub(super) struct RclOp {}
impl ShiftingOp for RclOp {
    const UPDATE_SZP: bool = false;
    const ZERO_SHIFT_UPDATES_CARRY: bool = false;
    const MASKED_COUNT_UPDATES_OF: bool = true;
    fn op(left: u64, _right: u64, count: u32, flags: RFlags, operand_bit_size: u32) -> u64 {
        let result = (left << count)
            | ((flags.carry() as u64) << (count - 1))
            | ((left as u128) >> ((operand_bit_size + 1) - count)) as u64; // add 1 for participation of cf
        sign_extend(result, operand_bit_size)
    }

    fn mod_count(masked_count: u32, operand_bit_size: u32) -> u32 {
        rcx_mod_count(masked_count, operand_bit_size)
    }

    fn carry_flag(left: u64, _right: u64, _result: u64, count: u32, operand_bit_size: u32) -> bool {
        ((left >> (operand_bit_size - count)) & LSB_MASK) != 0
    }

    fn overflow_flag(_left: u64, result: u64, carry_flag: bool, operand_bit_size: u32) -> bool {
        rotate_left_overflow(result, carry_flag, operand_bit_size)
    }
}

pub(super) struct RcrOp {}
impl ShiftingOp for RcrOp {
    const UPDATE_SZP: bool = false;
    const ZERO_SHIFT_UPDATES_CARRY: bool = false;
    const MASKED_COUNT_UPDATES_OF: bool = true;
    fn op(left: u64, _right: u64, count: u32, flags: RFlags, operand_bit_size: u32) -> u64 {
        let result = (left >> count)
            | ((flags.carry() as u64) << (operand_bit_size - count))
            | ((left as u128) << ((operand_bit_size + 1) - count)) as u64; // add 1 for participation of cf
        sign_extend(result, operand_bit_size)
    }

    fn mod_count(masked_count: u32, operand_bit_size: u32) -> u32 {
        rcx_mod_count(masked_count, operand_bit_size)
    }

    fn carry_flag(
        left: u64,
        _right: u64,
        _result: u64,
        count: u32,
        _operand_bit_size: u32,
    ) -> bool {
        ((left >> (count - 1)) & LSB_MASK) != 0
    }

    fn overflow_flag(_left: u64, result: u64, carry_flag: bool, operand_bit_size: u32) -> bool {
        rotate_right_overflow(result, carry_flag, operand_bit_size)
    }
}

/// Returns the mask for getting the most significant bit
fn msb_mask(operand_bit_size: u32) -> u64 {
    match operand_bit_size {
        8 => 0x80,
        16 => 0x8000,
        32 => 0x80000000,
        64 => 0x8000000000000000,
        _ => unreachable!(),
    }
}

fn rcx_mod_count(masked_count: u32, operand_bit_size: u32) -> u32 {
    if operand_bit_size == 8 {
        masked_count % 9
    } else if operand_bit_size == 16 {
        masked_count % 17
    } else {
        masked_count
    }
}

fn rox_mod_count(masked_count: u32, operand_bit_size: u32) -> u32 {
    masked_count % operand_bit_size
}

/// Truncates and sign-extends the result to the correct operand size
fn sign_extend(value: u64, operand_bit_size: u32) -> u64 {
    let sign_shift = 64 - operand_bit_size;
    (((value as i64) << sign_shift) >> sign_shift) as u64
}

/// Adjusts the count for shift instructions
fn shift_count(right: u64, operand_bit_size: u32) -> u32 {
    (if operand_bit_size == 64 {
        right & 0x3f
    } else {
        right & 0x1f
    }) as u32
}

/// Calculates the overflow flag for a right rotation
fn rotate_right_overflow(result: u64, _carry_flag: bool, operand_bit_size: u32) -> bool {
    let mask = msb_mask(operand_bit_size);
    let msb = (result & mask) != 0;
    let second_msb = (result & (mask >> 1)) != 0;
    msb ^ second_msb
}

/// Calculates the overflow flag for a left rotation
fn rotate_left_overflow(result: u64, carry_flag: bool, operand_bit_size: u32) -> bool {
    ((msb_mask(operand_bit_size) & result) != 0) ^ carry_flag
}
